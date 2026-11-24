//! P05: can a process-level checkpoint (CRIU) of a running headless shell, stopped under paused virtual
//! time, be restored into independent children that continue the saved execution? The page keeps a
//! closure, a pending timer and a dedicated worker's heap that no DOM, storage or pixel copy carries (the
//! adapter's P05 oracle, `crates/adapter-chromium/tests/checkpoint.rs`).
//!
//! The source runs in its own PID and network namespaces with its fixture server, so each restore gets
//! fresh namespaces and the children run beside the still-running source. DevTools is reached through a
//! host listener whose connections are opened inside the instance's network namespace.
use cdp::protocol::{emulation::VirtualTimePolicy, page, runtime, target};
use spike_common::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const DEVTOOLS: u16 = 9333;
const FIXTURES: u16 = 8080;
const IMAGES: &str = "/tmp/s19-images";
/// Everything the instance writes to disk (profile, log). The process image does not carry file contents,
/// so each child gets its own copy, taken while the source is frozen, mounted at this same path.
const INSTANCE: &str = "/tmp/s19-instance";
const SNAPSHOT: &str = "/tmp/s19-snapshot";

/// The namespace init (a shell) running the fixture server and the headless shell; returns the init's
/// host PID. CRIU refuses a tree whose session leader is outside its PID namespace, so the init leads its
/// own session.
fn launch(binary: &Path, fixtures: &Path) -> anyhow::Result<u32> {
    let _ = std::fs::remove_dir_all(INSTANCE);
    std::fs::create_dir_all(INSTANCE)?;
    let script = format!(
        "ip link set lo up && cd {fixtures} && python3 -m http.server {FIXTURES} --bind 127.0.0.1 >/dev/null 2>&1 & \
         setsid {binary} --no-sandbox --disable-gpu --remote-debugging-port={DEVTOOLS} --user-data-dir={INSTANCE}/profile about:blank \
         >{INSTANCE}/browser.log 2>&1 & wait",
        fixtures = fixtures.display(),
        binary = binary.display(),
    );
    let unshare = Command::new("unshare").args(["--pid", "--net", "--fork", "setsid", "sh", "-c", &script]).stdin(Stdio::null()).spawn()?;
    for _ in 0..100 {
        let out = Command::new("pgrep").args(["-P", &unshare.id().to_string()]).output()?;
        if let Some(pid) = String::from_utf8_lossy(&out.stdout).lines().next().and_then(|l| l.trim().parse().ok()) {
            return Ok(pid);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    anyhow::bail!("the namespace init did not start")
}

/// Opens a TCP connection to `port` inside `pid`'s network namespace. A socket keeps the namespace it was
/// created in, so only this short-lived thread enters it.
fn connect_in(pid: u32, port: u16) -> anyhow::Result<TcpStream> {
    std::thread::spawn(move || -> anyhow::Result<TcpStream> {
        enter_network(&std::fs::File::open(format!("/proc/{pid}/ns/net"))?)?;
        Ok(TcpStream::connect(("127.0.0.1", port))?)
    })
    .join()
    .map_err(|_| anyhow::anyhow!("connect thread panicked"))?
}

#[cfg(target_os = "linux")]
fn enter_network(ns: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    if unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNET) } != 0 { return Err(std::io::Error::last_os_error()) }
    Ok(())
}

/// The spike runs in a Linux container (`run.sh`); other hosts only build it.
#[cfg(not(target_os = "linux"))]
fn enter_network(_: &std::fs::File) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "network namespaces need Linux"))
}

/// A host port forwarding each connection to the instance's DevTools port.
fn forward(pid: u32) -> anyhow::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let Ok(inner) = connect_in(pid, DEVTOOLS) else { continue };
            let (mut a, mut b) = (client.try_clone().unwrap(), inner.try_clone().unwrap());
            let (mut c, mut d) = (client, inner);
            std::thread::spawn(move || { let _ = std::io::copy(&mut a, &mut b); let _ = b.shutdown(std::net::Shutdown::Both); });
            std::thread::spawn(move || { let _ = std::io::copy(&mut d, &mut c); let _ = c.shutdown(std::net::Shutdown::Both); });
        }
    });
    Ok(port)
}

/// The browser's WebSocket URL once DevTools answers through the forwarder.
fn ws_url(port: u16) -> anyhow::Result<String> {
    for _ in 0..300 {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            // The DevTools HTTP server answers an HTTP/1.0 request with an empty reply and keeps an HTTP/1.1
            // connection open, so the body is read by its Content-Length. The returned URL takes its authority
            // from the Host header.
            s.set_read_timeout(Some(Duration::from_secs(2)))?;
            s.write_all(format!("GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes())?;
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(n) = s.read(&mut chunk) {
                if n == 0 { break; }
                bytes.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&bytes);
                let Some((head, body)) = text.split_once("\r\n\r\n") else { continue };
                let length = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>()));
                if let Some(Ok(length)) = length && body.len() >= length {
                    let v: serde_json::Value = serde_json::from_str(&body[..length])?;
                    if let Some(url) = v["webSocketDebuggerUrl"].as_str() {
                        return Ok(url.to_string());
                    }
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("DevTools did not answer through port {port}:\n{}", tail(&format!("{INSTANCE}/browser.log"), 20))
}

/// Attaches to the oracle's page (the one already open after a restore, or a new one).
async fn page(client: &cdp::Client, url_part: &str) -> anyhow::Result<Page> {
    let targets = client.call(None, &target::GetTargetsParams::default()).await?.target_infos;
    let target_id = match targets.into_iter().find(|t| t.r#type == "page" && t.url.contains(url_part)) {
        Some(t) => t.target_id,
        None => client.call(None, &target::CreateTargetParams::new("about:blank".into())).await?.target_id,
    };
    let attached = client.call(None, &target::AttachToTargetParams { flatten: Some(true), ..target::AttachToTargetParams::new(target_id.clone()) }).await?;
    let p = Page { session: attached.session_id.0, target_id: target_id.0 };
    client.call(Some(&p.session), &page::EnableParams::default()).await?;
    client.call(Some(&p.session), &runtime::EnableParams::default()).await?;
    Ok(p)
}

async fn outputs(client: &cdp::Client, p: &Page) -> anyhow::Result<serde_json::Value> {
    eval(client, p, "({ready: document.querySelector('#ready').textContent, timer: document.querySelector('#timer').textContent, \
                     worker: document.querySelector('#worker').textContent, now: performance.now(), date: Date.now()})").await
}

/// The continuation a later grant exposes: `extra` further hidden steps, 400 virtual ms (the armed timer
/// reads the closure), then the worker's heap read by message.
async fn continue_and_read(client: &cdp::Client, p: &Page, extra: usize) -> anyhow::Result<serde_json::Value> {
    set_policy(client, p, VirtualTimePolicy::Pause, None).await?;
    let before = outputs(client, p).await?;
    for _ in 0..extra { eval(client, p, "document.querySelector('#step').click()").await?; }
    advance(client, p, 400.0).await?;
    eval(client, p, "document.querySelector('#read').click()").await?;
    // The stock engine delivers the worker's reply to the page only while virtual time advances.
    for _ in 0..40 {
        if outputs(client, p).await?["worker"] != "pending" { break; }
        tokio::time::sleep(Duration::from_millis(50)).await;
        advance(client, p, 5.0).await?;
    }
    Ok(serde_json::json!({ "extra_steps": extra, "before": before, "after": outputs(client, p).await? }))
}

/// One named phase with a wall deadline, logged so a stall names its phase.
async fn phase<T>(what: &str, fut: impl std::future::Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    eprintln!("s19: {what}");
    tokio::time::timeout(Duration::from_secs(40), fut).await.map_err(|_| anyhow::anyhow!("{what} stalled"))?
}

/// Connects through a fresh forwarder and runs the continuation.
async fn continuation(pid: u32, extra: usize) -> anyhow::Result<serde_json::Value> {
    let port = forward(pid)?;
    let client = cdp::Client::connect(&ws_url(port)?).await?;
    let p = page(&client, "index.html").await?;
    continue_and_read(&client, &p, extra).await
}

fn criu(args: &[&str]) -> anyhow::Result<(bool, String)> {
    output(Command::new("criu").args(args))
}

/// Dumps from inside the source's network namespace: CRIU then treats it as external and leaves it out.
/// (A fresh namespace on this kernel holds fallback tunnel devices CRIU cannot dump.) The post-dump action
/// runs before the frozen source resumes and copies its instance directory.
fn criu_dump(pid: u32, args: &[&str]) -> anyhow::Result<(bool, String)> {
    let script = "/tmp/s19-post-dump.sh";
    std::fs::write(script, format!("#!/bin/sh\n[ \"$CRTOOLS_SCRIPT_ACTION\" = post-dump ] || exit 0\nrm -rf {SNAPSHOT} && cp -a {INSTANCE} {SNAPSHOT}\n"))?;
    Command::new("chmod").args(["+x", script]).status()?;
    output(Command::new("nsenter").arg(format!("--net=/proc/{pid}/ns/net")).args(["criu", "dump", "--action-script", script]).args(args))
}

/// Restores child `n` into fresh network and mount namespaces, with its own copy of the instance directory
/// at the source's path; the restored tasks reopen their sockets and files there.
fn criu_restore(n: usize, args: &[&str]) -> anyhow::Result<(bool, String)> {
    let copy = format!("/tmp/s19-child-{n}");
    let _ = std::fs::remove_dir_all(&copy);
    Command::new("cp").args(["-a", SNAPSHOT, &copy]).status()?;
    let setup = format!("ip link set lo up && mount --bind {copy} {INSTANCE} && exec criu restore \"$@\"");
    output(Command::new("unshare").args(["--net", "--mount", "--propagation", "private", "sh", "-c", &setup, "sh"]).args(args))
}

fn output(command: &mut Command) -> anyhow::Result<(bool, String)> {
    let out = command.output()?;
    Ok((out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
}

/// A CRIU log's error and warning lines, which precede its long cleanup trace.
fn errors(path: &str) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines().filter(|l| l.contains("Error") || l.contains("Warn")).take(25).collect::<Vec<_>>().join("\n")
}

fn tail(path: &str, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Ends an instance: killing its namespace init ends every task in the PID namespace.
fn kill(pid: u32) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
    let pin = browser_pin::load_pin(&root)?;
    let binary = browser_pin::install(&root, &pin, browser_pin::current_platform()?)?;
    let url = format!("http://127.0.0.1:{FIXTURES}/index.html");

    // The source: two hidden steps, then an armed timer, stopped under paused virtual time.
    let source = launch(&binary, &fixtures)?;
    let port = forward(source)?;
    let client = phase("connect", async { Ok(cdp::Client::connect(&ws_url(port)?).await?) }).await?;
    let p = phase("page", page(&client, "index.html")).await?;
    phase("pause", set_policy(&client, &p, VirtualTimePolicy::Pause, None)).await?;
    phase("navigate", navigate(&client, &p, &url)).await?;
    for _ in 0..20 {
        phase("load", advance(&client, &p, 50.0)).await?;
        if phase("ready", outputs(&client, &p)).await?["ready"] == "ready" { break; }
    }
    phase("steps", async {
        for _ in 0..2 { eval(&client, &p, "document.querySelector('#step').click()").await?; }
        eval(&client, &p, "document.querySelector('#arm').click()").await?;
        advance(&client, &p, 50.0).await
    }).await?;
    let stopped = phase("stopped", outputs(&client, &p)).await?;
    drop(client);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let _ = std::fs::remove_dir_all(IMAGES);
    std::fs::create_dir_all(IMAGES)?;
    let started = std::time::Instant::now();
    let source_pid = source.to_string();
    let (dumped, dump_out) = criu_dump(source, &["-t", &source_pid, "-D", IMAGES, "--leave-running", "--tcp-close", "--file-locks",
        "--irmap-scan-path", "/etc", "--irmap-scan-path", "/usr/share/zoneinfo", "-v4", "-o", "dump.log"])?;
    let dump_ms = started.elapsed().as_millis();
    let image_bytes: u64 = std::fs::read_dir(IMAGES)?.filter_map(|e| e.ok()?.metadata().ok()).map(|m| m.len()).sum();

    // Two children from the one image, alive together beside the still-running source.
    let mut children = Vec::new();
    if dumped {
        for n in 1..=2 {
            let (log, pidfile) = (format!("restore-{n}.log"), format!("/tmp/s19-child-{n}.pid"));
            let started = std::time::Instant::now();
            let (restored, restore_out) = criu_restore(n, &["-D", IMAGES, "--restore-detached", "--tcp-close", "--file-locks",
                "--pidfile", &pidfile, "-v4", "-o", &log])?;
            let restore_ms = started.elapsed().as_millis();
            let pid: Option<u32> = std::fs::read_to_string(&pidfile).ok().and_then(|s| s.trim().parse().ok());
            children.push((n, restored, restore_ms, pid, if restored { None } else { Some((restore_out, errors(&format!("{IMAGES}/{log}")))) }));
        }
    }
    // The first child takes one more hidden step; the second child and the source take none.
    let mut reports = Vec::new();
    for (n, restored, restore_ms, pid, failure) in &children {
        let continuation = match (restored, pid) {
            (true, Some(pid)) => phase(&format!("child {n} continuation"), continuation(*pid, if *n == 1 { 1 } else { 0 })).await
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            _ => serde_json::Value::Null,
        };
        reports.push(serde_json::json!({
            "restored": restored, "restore_ms": restore_ms, "host_pid": pid, "continuation": continuation,
            "criu": failure.as_ref().map(|(out, log)| serde_json::json!({ "out": out, "log": log })),
        }));
    }
    let source_continuation = phase("source continuation", continuation(source, 0)).await
        .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }));
    for (_, _, _, pid, _) in &children { if let Some(pid) = pid { kill(*pid); } }
    kill(source);

    report("s19-criu-checkpoint", serde_json::json!({
        "criu": criu(&["--version"])?.1.trim(),
        "stopped": stopped,
        "dump": { "ok": dumped, "ms": dump_ms, "image_bytes": image_bytes,
                  "failure": if dumped { serde_json::Value::Null } else { serde_json::json!({ "out": dump_out, "log": errors(&format!("{IMAGES}/dump.log")) }) } },
        "children": reports,
        "source": source_continuation,
    }));
    Ok(())
}
