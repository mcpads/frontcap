use super::*;
use std::io::{Read, Write};

/// A directory only its creator may enter, as a cell's directory needs for its parent.
fn private() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn browser() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let pin = crate::load_pin(&root).unwrap();
    crate::install(&root, &pin, crate::current_platform().unwrap()).unwrap()
}

/// The browser in a cell answers DevTools on the cell's loopback only, runs in the cell's PID namespace
/// with its profile in the cell's directory, and ends with the cell.
#[test]
#[ignore = "creates namespaces: run as root on Linux with --ignored"]
fn a_browser_in_a_cell_is_reached_only_through_the_cell() {
    let base = private();
    let dir = base.path().join("cell");
    let user = CellUser { uid: 65534, gid: 65534 };
    // A cell's directory whose parent others may enter is refused: a browser could reach it by its path.
    assert!(matches!(Cell::create(&Path::new("/tmp").join("frontcap-cell-open"), Path::new("/run/frontcap-cell-test"), user), Err(LaunchError::Unsupported(_))));
    assert!(matches!(Cell::create(&dir, Path::new("/run/frontcap-cell-test"), CellUser { uid: 0, gid: 0 }), Err(LaunchError::Unsupported(_))));
    // A mount point anyone can redirect is refused: in a directory others can write, or a link.
    assert!(matches!(Cell::create(&dir, Path::new("/tmp/frontcap-cell-test"), user), Err(LaunchError::Unsupported(_))));
    let _ = std::fs::remove_file("/run/frontcap-cell-link");
    std::os::unix::fs::symlink("/etc", "/run/frontcap-cell-link").unwrap();
    assert!(matches!(Cell::create(&dir, Path::new("/run/frontcap-cell-link"), user), Err(LaunchError::Unsupported(_))));
    std::fs::remove_file("/run/frontcap-cell-link").unwrap();
    let cell = Cell::create(&dir, Path::new("/run/frontcap-cell-test"), user).unwrap();
    let ns = |pid: &str, kind: &str| std::fs::read_link(format!("/proc/{pid}/ns/{kind}")).unwrap();
    for kind in ["pid", "net", "mnt"] {
        assert_ne!(ns(&cell.init.to_string(), kind), ns("self", kind), "{kind}");
    }
    let started = cell.start(&browser(), &["--headless".into()], Duration::from_secs(30)).unwrap();
    assert!(dir.join("profile/DevToolsActivePort").is_file(), "the profile is in the cell's directory");
    // Only the profile is the cell user's: the start file and the directory stay the creator's.
    use std::os::unix::fs::MetadataExt as _;
    assert_eq!(std::fs::metadata(dir.join("profile")).unwrap().uid(), 65534);
    assert_eq!(std::fs::metadata(&dir).unwrap().uid(), 0);
    assert_eq!(std::fs::metadata(dir.join(START)).unwrap().uid(), 0);
    let mut s = cell.connect(started.port).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(format!("GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", started.port).as_bytes()).unwrap();
    let mut reply = vec![0; 4096];
    let n = s.read(&mut reply).unwrap();
    assert!(String::from_utf8_lossy(&reply[..n]).contains("webSocketDebuggerUrl"), "{}", String::from_utf8_lossy(&reply[..n]));
    // The browser runs as the cell's user, with no capabilities and none to gain.
    let browsers: Vec<String> = std::fs::read_dir("/proc").unwrap().flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()))
        .filter(|pid| std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains("--remote-debugging-port")))
        .filter_map(|pid| std::fs::read_to_string(format!("/proc/{pid}/status")).ok())
        .collect();
    assert!(!browsers.is_empty());
    for status in &browsers {
        let field = |name: &str| status.lines().find_map(|l| l.strip_prefix(name)).map(str::trim).unwrap_or_default().to_string();
        assert!(field("Uid:").split_whitespace().all(|u| u == "65534"), "{status}");
        assert_eq!(field("CapEff:"), "0000000000000000", "{status}");
        assert_eq!(field("CapBnd:"), "0000000000000000", "{status}");
        assert_eq!(field("NoNewPrivs:"), "1", "{status}");
    }
    let inside = cell.bind(0).unwrap().local_addr().unwrap();
    assert!(std::net::TcpStream::connect(inside).is_err(), "a listener in the cell is not reachable from here");
    cell.end();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(cell.init) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(cell.init), "the cell ended");
}

fn version(cell: &Cell, port: u16) -> String {
    let mut s = cell.connect(port).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(format!("GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
    let mut reply = vec![0; 4096];
    let n = s.read(&mut reply).unwrap();
    String::from_utf8_lossy(&reply[..n]).into_owned()
}

/// A cell's image restores into two children at once beside the running source, each in namespaces of
/// its own with its own copy of the cell's directory, each browser answering on its own loopback at the
/// source's port; a listener bound before the restore is there for the child.
#[test]
#[ignore = "creates namespaces and runs CRIU: run as root on Linux with --ignored"]
fn a_cells_image_restores_into_children_beside_the_running_source() {
    let dir = private();
    let user = CellUser { uid: 65534, gid: 65534 };
    let mount = Path::new("/run/frontcap-cell-image-test");
    let source = Cell::create(&dir.path().join("source"), mount, user).unwrap();
    let started = source.start(&browser(), &["--headless".into(), "--disable-gpu".into()], Duration::from_secs(30)).unwrap();
    assert!(version(&source, started.port).contains("webSocketDebuggerUrl"));
    let image = source.image(&dir.path().join("image")).unwrap();
    assert!(image.dir.join("profile").is_dir(), "the cell's directory is copied with the image");
    let restoring: Vec<Restoring> = (1..=2).map(|n| Restoring::prepare(&image, &dir.path().join(format!("child-{n}")), mount).unwrap()).collect();
    let listeners: Vec<_> = restoring.iter().map(|r| r.bind(started.port + 1).unwrap()).collect();
    let children: Vec<Cell> = restoring.into_iter().map(|r| r.restore(&image).unwrap()).collect();
    let ns = |pid: u32, kind: &str| std::fs::read_link(format!("/proc/{pid}/ns/{kind}")).unwrap();
    for child in &children {
        assert!(version(child, started.port).contains("webSocketDebuggerUrl"), "the restored browser answers");
        for kind in ["pid", "net", "mnt"] {
            assert_ne!(ns(child.init, kind), ns(source.init, kind), "{kind}");
        }
    }
    assert_ne!(ns(children[0].init, "net"), ns(children[1].init, "net"));
    assert!(children[0].connect(started.port + 1).is_ok(), "the listener bound before the restore is the child's");
    assert!(version(&source, started.port).contains("webSocketDebuggerUrl"), "the source goes on");
    drop(listeners);
    for cell in children.iter().chain([&source]) {
        cell.end();
    }
}
