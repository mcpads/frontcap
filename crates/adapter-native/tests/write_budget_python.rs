//! Cross-language writer admission belongs to the native Python integration boundary.
use frontcap_core::{
    ids::LaunchId,
    write_budget::{Class, Limits, WriteBudget},
};
use std::{fs, path::Path};

#[test]
fn python_and_rust_share_charges_and_retirement_without_reset() {
    use std::process::Command;
    let root = tempfile::tempdir().unwrap();
    let limits = Limits {
        ordinary: 64,
        retirement: 64,
    };
    let budget =
        WriteBudget::create(&root.path().join("budget"), LaunchId::generate(), limits).unwrap();
    let tools = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools");
    let script = r#"
import errno,json,sys,time
from pathlib import Path
from native_write_budget import charge_payload,begin_retirement
from native_host_process import write_json
root=Path(sys.argv[1]);(root/'ready').touch()
while not (root/'go').exists():time.sleep(.001)
accepted=0
for _ in range(64):
 try:charge_payload(1);accepted+=1
 except OSError as error:
  assert error.errno==errno.ENOSPC
(root/'python-count').write_text(str(accepted))
try:write_json(root/'refused.json',{'message':'ordinary exhausted'})
except OSError as error:assert error.errno==errno.ENOSPC
else:raise AssertionError('ordinary write should be refused')
begin_retirement();write_json(root/'terminal.json',{'retired':True})
"#;
    let mut command = Command::new("python3");
    command
        .arg("-c")
        .arg(script)
        .arg(root.path())
        .env("PYTHONPATH", tools);
    WriteBudget::configure_child(&mut command, Some(&budget)).unwrap();
    let mut child = command.spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !root.path().join("ready").exists() {
        if child.try_wait().unwrap().is_some() || std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Python budget worker did not become ready");
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    fs::write(root.path().join("go"), b"").unwrap();
    let mut accepted = 0;
    for _ in 0..64 {
        if budget.charge(Class::Ordinary, 1).is_ok() {
            accepted += 1;
        }
    }
    assert!(child.wait().unwrap().success());
    let python_count: u64 = fs::read_to_string(root.path().join("python-count"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(accepted + python_count, limits.ordinary);
    let spent = budget.charged().unwrap();
    assert_eq!(spent.ordinary, limits.ordinary);
    assert_eq!(
        spent.retirement,
        fs::metadata(root.path().join("terminal.json"))
            .unwrap()
            .len()
    );
    assert!(!root.path().join("refused.json").exists());
    assert!(!root.path().join("refused.tmp").exists());
    budget
        .charge(Class::Retirement, limits.retirement - spent.retirement)
        .unwrap();
    assert!(budget.charge(Class::Retirement, 1).is_err());
    let mut missing = Command::new("python3");
    missing.arg("-c").arg("from native_write_budget import charge_payload\ntry:charge_payload(1)\nexcept FileNotFoundError:pass\nelse:raise AssertionError('missing ledger must fail closed')")
        .env("PYTHONPATH", Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools"));
    WriteBudget::configure_child(&mut missing, Some(&budget)).unwrap();
    fs::rename(
        root.path().join("budget/ledger.json"),
        root.path().join("ledger.saved"),
    )
    .unwrap();
    assert!(missing.status().unwrap().success());
    assert!(!root.path().join("budget/ledger.json").exists());
}
