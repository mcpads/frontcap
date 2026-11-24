//! A browser cell (Linux; elsewhere its operations are unsupported): the browser runs under a namespace init in its own PID, network and mount
//! namespaces, with the cell's directory mounted at a fixed path, so its process tree, sockets and files
//! can be imaged and restored apart from the host's. Creating one needs the privileges
//! to create those namespaces and mount (root on a Linux host).
//!
//! The browser is the init's descendant, as a process image of the tree needs it; it starts only once the
//! caller has bound what it must offer inside the cell (its proxy's listener), since the browser is told
//! that address at launch. The init keeps the privileges it mounted with; the browser runs as the cell's
//! unprivileged user, with no capabilities and no way to gain any, since it runs without the engine's own
//! sandbox.
use crate::launch::{LaunchError, base_args, parse_active_port};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The file the init waits for: the browser's command line, one argument per line.
const START: &str = ".frontcap-start";

/// The unprivileged user and group a cell's browser runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CellUser {
    pub uid: u32,
    pub gid: u32,
}

/// A cell by its init and directories, recorded so a later process reaches the same browser.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Cell {
    /// The namespace init, by its host PID: ending it ends every task of the cell.
    pub init: u32,
    /// The cell's directory on the host.
    pub dir: PathBuf,
    /// Where the cell sees it.
    pub mount: PathBuf,
}

/// What `Cell::start` found: the browser's DevTools port inside the cell and its WebSocket path.
#[derive(Debug)]
pub struct Started {
    pub port: u16,
    pub path: String,
}

impl Cell {
    /// Creates the namespaces, brings their loopback up and mounts `dir` at `mount`; the init then waits
    /// for `start`, whose browser runs as `user` (root is refused; a user of its own per cell keeps cells
    /// apart). Only `profile/` is the user's: the start file the privileged init reads, its working
    /// directory `init/`, and everything else the caller writes, stay where the browser cannot plant a link.
    pub fn create(dir: &Path, mount: &Path, user: CellUser) -> Result<Cell, LaunchError> {
        if !cfg!(target_os = "linux") {
            return Err(LaunchError::Unsupported("browser cells need Linux".into()));
        }
        if user.uid == 0 || user.gid == 0 {
            return Err(LaunchError::Unsupported("a cell's browser runs without the engine's sandbox, so never as root".into()));
        }
        mount_point(mount)?;
        private_parent(dir)?;
        std::fs::create_dir_all(dir.join("profile")).map_err(LaunchError::Profile)?;
        let _ = std::fs::remove_file(dir.join(START));
        own(&dir.join("profile"), user)?;
        private_dir(&dir.join("init"))?;
        // Every task of the cell works in a directory of the cell's own. The cell sees the host's `/proc`
        // (CRIU, run in the cell's mount namespace, needs its own tasks there), so CRIU's parasite mounts a
        // `/proc` of the cell's at a fixed name in each task's working directory: two cells imaged at once
        // must not share one. The init, which mounts as root, works where the browser cannot plant a link.
        let script = format!(
            "ip link set lo up && mount --bind '{dir}' '{mount}' && cd '{mount}/init' || exit 1; \
             while [ ! -e '{mount}/{START}' ]; do sleep 0.01; done; \
             xargs -d '\\n' -a '{mount}/{START}' setpriv --reuid={uid} --regid={gid} --clear-groups \
               --inh-caps=-all --bounding-set=-all --no-new-privs \
               sh -c 'cd \"{mount}/profile\" && exec \"$@\" >/dev/null 2>&1' sh & wait",
            dir = dir.display(),
            mount = mount.display(),
            uid = user.uid,
            gid = user.gid,
        );
        let unshare = Command::new("unshare")
            .args(["--pid", "--net", "--mount", "--propagation", "private", "--fork", "setsid", "sh", "-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| LaunchError::Spawn { binary: "unshare".into(), source })?;
        let parent = unshare.id();
        // Reap `unshare` when the cell ends; never kill it on drop.
        std::thread::spawn(move || {
            let mut unshare = unshare;
            let _ = unshare.wait();
        });
        let start = Instant::now();
        loop {
            if let Some(init) = child_of(parent) {
                return Ok(Cell { init, dir: dir.to_path_buf(), mount: mount.to_path_buf() });
            }
            if start.elapsed() > Duration::from_secs(10) {
                return Err(LaunchError::Timeout(Duration::from_secs(10)));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Starts the browser with its profile in the cell's directory and waits for DevTools to listen.
    pub fn start(&self, binary: &Path, extra_args: &[String], timeout: Duration) -> Result<Started, LaunchError> {
        let profile = self.mount.join("profile");
        let marker = self.dir.join("profile/DevToolsActivePort");
        let mut args = vec![binary.display().to_string()];
        // Both of the engine's layer-1 sandboxes nest PID namespaces, which a process image cannot hold;
        // the cell's namespaces isolate the browser instead.
        args.push("--no-sandbox".into());
        args.extend(extra_args.iter().cloned());
        args.extend(base_args(&profile));
        // The start file holds one argument per line: a line break inside one would add arguments.
        if let Some(arg) = args.iter().find(|a| a.contains(['\n', '\0'])) {
            return Err(LaunchError::Unsupported(format!("a browser argument with a line break: {arg:?}")));
        }
        let staged = self.dir.join(format!("{START}.partial"));
        std::fs::write(&staged, args.join("\n") + "\n").map_err(LaunchError::Profile)?;
        std::fs::rename(&staged, self.dir.join(START)).map_err(LaunchError::Profile)?;
        let start = Instant::now();
        loop {
            if !alive(self.init) {
                return Err(LaunchError::ExitedEarly { status: "the cell ended".into(), detail: String::new() });
            }
            if let Some(text) = read_regular(&marker)
                && let Ok((port, path)) = parse_active_port(&text)
            {
                return Ok(Started { port, path });
            }
            if start.elapsed() > timeout {
                return Err(LaunchError::Timeout(timeout));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Opens a TCP connection to `port` on the cell's loopback.
    pub fn connect(&self, port: u16) -> std::io::Result<std::net::TcpStream> {
        self.inside(move || std::net::TcpStream::connect(("127.0.0.1", port)))
    }

    /// Binds a listener on the cell's loopback.
    pub fn bind(&self, port: u16) -> std::io::Result<std::net::TcpListener> {
        self.inside(move || std::net::TcpListener::bind(("127.0.0.1", port)))
    }

    /// Ends every task of the cell.
    pub fn end(&self) {
        unsafe { libc::kill(self.init as i32, libc::SIGKILL) };
    }

    fn inside<T: Send + 'static>(&self, f: impl FnOnce() -> std::io::Result<T> + Send + 'static) -> std::io::Result<T> {
        inside(self.init, f)
    }

    /// Takes a process image of the cell, which goes on running: CRIU's images under `to/images`, and a
    /// copy of the cell's directory under `to/cell` made while the tree is frozen. CRIU runs inside the
    /// cell's network and mount namespaces, so only its PID namespace enters the image; each child gets
    /// fresh ones (`Restoring`). The caller has closed its own connections into the cell.
    pub fn image(&self, to: &Path) -> Result<Image, LaunchError> {
        let image = Image { images: to.join("images"), dir: to.join("cell") };
        std::fs::create_dir_all(&image.images).map_err(LaunchError::Profile)?;
        let script = to.join("post-dump.sh");
        std::fs::write(&script, format!(
            "#!/bin/sh\n[ \"$CRTOOLS_SCRIPT_ACTION\" = post-dump ] || exit 0\nexec cp -a -- '{}' '{}'\n",
            self.dir.display(), image.dir.display(),
        )).map_err(LaunchError::Profile)?;
        executable(&script)?;
        let init = self.init.to_string();
        criu(&init, &image.images, &image.images, "dump.log", &[
            "dump", "-t", &init, "--leave-running", "--tcp-close", "--file-locks",
            // The engine watches files with inotify; the container's overlay root cannot reopen their handles.
            "--irmap-scan-path", "/etc", "--irmap-scan-path", "/usr/share/zoneinfo",
            "--action-script", &script.display().to_string(),
        ])?;
        Ok(image)
    }
}

/// A cell's process image: CRIU's images and the cell's directory as it was.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Image {
    pub images: PathBuf,
    pub dir: PathBuf,
}

/// Fresh network and mount namespaces for a child of an image, held open while the caller binds what it
/// offers inside (its proxy's listener, at the port the browser was given) before the restore.
#[derive(Debug)]
pub struct Restoring {
    holder: u32,
    dir: PathBuf,
    mount: PathBuf,
}

impl Restoring {
    /// Copies the image's directory to `dir`, the child's own, and mounts it at `mount`, the cell's path,
    /// in new network and mount namespaces.
    pub fn prepare(image: &Image, dir: &Path, mount: &Path) -> Result<Restoring, LaunchError> {
        if !cfg!(target_os = "linux") {
            return Err(LaunchError::Unsupported("browser cells need Linux".into()));
        }
        mount_point(mount)?;
        private_parent(dir)?;
        if dir.exists() {
            return Err(LaunchError::Unsupported(format!("a child's cell directory {} exists already", dir.display())));
        }
        let copied = Command::new("cp").arg("-a").arg("--").arg(&image.dir).arg(dir).status().map_err(LaunchError::Profile)?;
        if !copied.success() {
            return Err(LaunchError::Checkpoint(format!("copying the image's directory to {} failed: {copied}", dir.display())));
        }
        let script = format!(
            "ip link set lo up && mount --bind '{dir}' '{mount}' && exec sleep infinity",
            dir = dir.display(),
            mount = mount.display(),
        );
        let unshare = Command::new("unshare")
            .args(["--net", "--mount", "--propagation", "private", "--fork", "sh", "-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| LaunchError::Spawn { binary: "unshare".into(), source })?;
        let parent = unshare.id();
        std::thread::spawn(move || {
            let mut unshare = unshare;
            let _ = unshare.wait();
        });
        let start = Instant::now();
        loop {
            // Ready once its mount is in place: the shell has become `sleep`.
            if let Some(holder) = child_of(parent)
                && std::fs::read_to_string(format!("/proc/{holder}/mounts")).is_ok_and(|m| m.lines().any(|l| l.split(' ').nth(1) == Some(&*mount.display().to_string())))
            {
                return Ok(Restoring { holder, dir: dir.to_path_buf(), mount: mount.to_path_buf() });
            }
            if start.elapsed() > Duration::from_secs(10) {
                return Err(LaunchError::Timeout(Duration::from_secs(10)));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Binds a listener on the child's loopback, before the restore.
    pub fn bind(&self, port: u16) -> std::io::Result<std::net::TcpListener> {
        inside(self.holder, move || std::net::TcpListener::bind(("127.0.0.1", port)))
    }

    /// Restores the image into these namespaces: a new cell, its init a new PID namespace's. The image is
    /// only read: CRIU's log, statistics and PID file go beside the child's directory.
    pub fn restore(self, image: &Image) -> Result<Cell, LaunchError> {
        let holder = self.holder.to_string();
        let work = self.dir.parent().unwrap_or(Path::new("/")).to_path_buf();
        let pidfile = work.join(format!("restored-{holder}.pid"));
        let _ = std::fs::remove_file(&pidfile);
        let restored = criu(&holder, &image.images, &work, &format!("restore-{holder}.log"), &[
            "restore", "--restore-detached", "--tcp-close", "--file-locks", "--pidfile", &pidfile.display().to_string(),
        ]);
        // The restored tasks keep the namespaces; the holder is no longer needed.
        unsafe { libc::kill(self.holder as i32, libc::SIGKILL) };
        restored?;
        let init = std::fs::read_to_string(&pidfile).ok().and_then(|p| p.trim().parse().ok())
            .ok_or_else(|| LaunchError::Checkpoint("the restore wrote no PID".into()))?;
        let _ = std::fs::remove_file(&pidfile);
        Ok(Cell { init, dir: self.dir, mount: self.mount })
    }
}

/// Runs CRIU inside `pid`'s network and mount namespaces (its own then, which it leaves out of an image),
/// on the images in `images`, logging to `log` in `work`; a failure carries the log's error lines.
fn criu(pid: &str, images: &Path, work: &Path, log: &str, args: &[&str]) -> Result<(), LaunchError> {
    let out = Command::new("nsenter")
        .args(["--target", pid, "--net", "--mount", "criu"])
        .args(args)
        .arg("-D").arg(images)
        .arg("-W").arg(work)
        .args(["-v4", "-o", log])
        .output()
        .map_err(|source| LaunchError::Spawn { binary: "criu".into(), source })?;
    if out.status.success() {
        return Ok(());
    }
    let logged = std::fs::read_to_string(work.join(log)).unwrap_or_default();
    let errors: Vec<&str> = logged.lines().filter(|l| l.contains("Error")).take(10).collect();
    Err(LaunchError::Checkpoint(format!("criu {} failed ({}): {}{}", args[0], out.status, String::from_utf8_lossy(&out.stderr).trim(), errors.join("\n"))))
}

fn executable(path: &Path) -> Result<(), LaunchError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(LaunchError::Profile)?;
    }
    Ok(())
}

/// Runs `f` on a thread that has entered `pid`'s network namespace: a socket keeps the namespace it was
/// made in, and only that thread enters it.
fn inside<T: Send + 'static>(pid: u32, f: impl FnOnce() -> std::io::Result<T> + Send + 'static) -> std::io::Result<T> {
    let ns = std::fs::File::open(format!("/proc/{pid}/ns/net"))?;
    std::thread::spawn(move || {
        enter_network(&ns)?;
        f()
    })
    .join()
    .map_err(|_| std::io::Error::other("the namespace thread panicked"))?
}

#[cfg(target_os = "linux")]
fn enter_network(ns: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    if unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn enter_network(_: &std::fs::File) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "network namespaces need Linux"))
}

/// Makes sure the privileged init mounts where no one else can redirect it: `mount` and its parent are
/// real directories (not links), the creator's, and nobody else's to write; `mount` is created if absent.
#[cfg(unix)]
fn mount_point(mount: &Path) -> Result<(), LaunchError> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
    let refuse = |why: String| Err(LaunchError::Unsupported(format!("a cell's mount point {}: {why}", mount.display())));
    let me = unsafe { libc::geteuid() };
    let safe = |path: &Path| -> Result<bool, LaunchError> {
        let m = std::fs::symlink_metadata(path).map_err(LaunchError::Profile)?;
        Ok(m.file_type().is_dir() && m.uid() == me && m.mode() & 0o022 == 0)
    };
    let Some(parent) = mount.parent().filter(|p| p.is_absolute()) else { return refuse("not an absolute path".into()) };
    if !safe(parent)? {
        return refuse(format!("{} is a link, someone else's, or writable by others", parent.display()));
    }
    // Created here unless it exists, as another cell's creator may have made it meanwhile.
    match std::fs::DirBuilder::new().mode(0o755).create(mount) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if safe(mount)? { Ok(()) } else { refuse("a link, someone else's, or writable by others".into()) }
        }
        Err(e) => refuse(e.to_string()),
    }
}

#[cfg(not(unix))]
fn mount_point(_: &Path) -> Result<(), LaunchError> {
    Err(LaunchError::Unsupported("browser cells need Linux".into()))
}

/// Makes sure a cell's directory is reached only through its mount: its parent is the creator's and nobody
/// else's to enter, so no cell's browser reaches any cell's directory by its host path (restored children
/// share their source's user, as the image holds it).
#[cfg(unix)]
fn private_parent(dir: &Path) -> Result<(), LaunchError> {
    use std::os::unix::fs::MetadataExt as _;
    let Some(parent) = dir.parent().filter(|p| p.is_absolute()) else {
        return Err(LaunchError::Unsupported(format!("a cell's directory {} is not an absolute path", dir.display())));
    };
    let m = std::fs::symlink_metadata(parent).map_err(LaunchError::Profile)?;
    if !(m.file_type().is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0) {
        return Err(LaunchError::Unsupported(format!("{}, which holds a cell's directory, must be the creator's alone (mode 0700, not a link)", parent.display())));
    }
    Ok(())
}

#[cfg(not(unix))]
fn private_parent(_: &Path) -> Result<(), LaunchError> {
    Err(LaunchError::Unsupported("browser cells need Linux".into()))
}

/// Creates `dir` as the creator's alone (mode 0700) unless it exists so already, not as a link.
#[cfg(unix)]
fn private_dir(dir: &Path) -> Result<(), LaunchError> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(LaunchError::Profile(e)),
    }
    let m = std::fs::symlink_metadata(dir).map_err(LaunchError::Profile)?;
    if !(m.file_type().is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0) {
        return Err(LaunchError::Unsupported(format!("{} must be the creator's alone (mode 0700, not a link)", dir.display())));
    }
    Ok(())
}

#[cfg(not(unix))]
fn private_dir(_: &Path) -> Result<(), LaunchError> {
    Err(LaunchError::Unsupported("browser cells need Linux".into()))
}

/// Gives `dir` itself (not what it may later hold) to the cell's user.
#[cfg(unix)]
fn own(dir: &Path, user: CellUser) -> Result<(), LaunchError> {
    std::os::unix::fs::lchown(dir, Some(user.uid), Some(user.gid)).map_err(LaunchError::Profile)
}

#[cfg(not(unix))]
fn own(_: &Path, _: CellUser) -> Result<(), LaunchError> {
    Err(LaunchError::Unsupported("browser cells need Linux".into()))
}

/// A file the cell's user can write, read only if it is a regular file there (not a link the browser
/// planted to make this process read another).
fn read_regular(path: &Path) -> Option<String> {
    use std::io::Read as _;
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path).ok()?
    };
    #[cfg(not(unix))]
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    file.take(4096).read_to_string(&mut text).ok()?;
    Some(text)
}

fn child_of(parent: u32) -> Option<u32> {
    let children = std::fs::read_to_string(format!("/proc/{parent}/task/{parent}/children")).ok()?;
    children.split_whitespace().next()?.parse().ok()
}

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'))
}

#[cfg(test)]
#[path = "cell_tests.rs"]
mod tests;
