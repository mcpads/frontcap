//! frontcap's patched engine: the pinned tag with the patch set in `engine/` (conventions.md, Pinned
//! engine). The patch set's identity is a digest of the pinned version, `engine/args.gn` and every
//! patch, so a build is used only for the patch set it was made from.
use crate::install::sha256_hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("the source checkout {src} is at {found}, not the pinned {base}")]
    WrongBase { src: String, found: String, base: String },
    #[error("the source checkout {0} has changes frontcap did not make; the build applies the patch set to a clean base only (`git -C {0} reset --hard` discards them)")]
    Dirty(String),
    #[error("the source checkout {0} has changes that are not exactly what an interrupted build recorded applying (edits since, or a build killed mid-patch); the build refuses to discard them: inspect them, then `git -C {0} reset --hard` if they are not yours")]
    Mixed(String),
    #[error("the patched build {dir} does not match its manifest ({detail}); rebuild it (`browser-pin engine build`)")]
    Corrupt { dir: String, detail: String },
    #[error("{step} failed: {detail}")]
    Step { step: String, detail: String },
    #[error("no build target for platform {0}")]
    UnknownPlatform(String),
    #[error(transparent)]
    Install(#[from] crate::install::InstallError),
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> EngineError + '_ {
    move |source| EngineError::Io { path: path.display().to_string(), source }
}

/// What a patched build holds, written last: a directory without it is not a build.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub base: String,
    pub patchset: String,
    pub platform: String,
    /// The browser executable, relative to the build's directory.
    pub binary: String,
    /// Every file's SHA-256, by path relative to the build's directory.
    pub files: BTreeMap<String, String>,
}

pub const MANIFEST: &str = "manifest.json";

fn engine_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("engine")
}

/// The patches, in the order they apply (by name).
pub fn patches(repo_root: &Path) -> Result<Vec<PathBuf>, EngineError> {
    patches_in(&engine_dir(repo_root).join("patches"))
}

fn patches_in(dir: &Path) -> Result<Vec<PathBuf>, EngineError> {
    let mut out: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "patch")).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(io(dir)(e)),
    };
    out.sort();
    Ok(out)
}

/// V8 is a separate checkout, so its patch and exact upstream commit travel together.
/// An absent directory preserves the identity of Chromium-only patch sets.
fn v8_patches(repo_root: &Path) -> Result<Option<(String, Vec<PathBuf>)>, EngineError> {
    let dir = engine_dir(repo_root).join("v8");
    if !dir.exists() { return Ok(None); }
    let path = dir.join("base-revision");
    let revision = std::fs::read_to_string(&path).map_err(io(&path))?.trim().to_string();
    let patches = patches_in(&dir)?;
    if revision.len() != 40 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) || patches.is_empty() {
        return Err(EngineError::Step { step: "reading V8 patches".into(), detail: "engine/v8 requires a full commit in base-revision and at least one .patch".into() });
    }
    Ok(Some((revision.to_ascii_lowercase(), patches)))
}

/// The patch set's identity: a digest over the base version, `args.gn`, and each patch's name and
/// content, in order, every field length-prefixed.
pub fn patchset_id(repo_root: &Path) -> Result<String, EngineError> {
    let base = crate::load_pin(repo_root).map_err(|e| EngineError::Step { step: "reading the pin".into(), detail: e.to_string() })?.version;
    let mut h = Sha256::new();
    let mut field = |bytes: &[u8]| {
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    };
    field(base.as_bytes());
    let args = engine_dir(repo_root).join("args.gn");
    field(&std::fs::read(&args).map_err(io(&args))?);
    for p in patches(repo_root)? {
        field(p.file_name().expect("a file").as_encoded_bytes());
        field(&std::fs::read(&p).map_err(io(&p))?);
    }
    if let Some((revision, patches)) = v8_patches(repo_root)? {
        field(b"v8");
        field(revision.as_bytes());
        for p in patches {
            field(p.file_name().expect("a file").as_encoded_bytes());
            field(&std::fs::read(&p).map_err(io(&p))?);
        }
    }
    let digest = h.finalize();
    Ok(digest.iter().take(8).map(|b| format!("{b:02x}")).collect())
}

pub fn patched_dir(repo_root: &Path, patchset: &str, platform: &str) -> PathBuf {
    repo_root.join("browsers").join("patched").join(patchset).join(platform)
}

/// The patched build of the current patch set on the pinned base, when it has been built: its executable
/// and manifest. A tree without a patch set (`engine/args.gn`) has none.
pub fn patched_binary(repo_root: &Path, platform: &str) -> Result<Option<(PathBuf, Manifest)>, EngineError> {
    if !engine_dir(repo_root).join("args.gn").is_file() {
        return Ok(None);
    }
    let id = patchset_id(repo_root)?;
    let base = crate::load_pin(repo_root).map_err(|e| EngineError::Step { step: "reading the pin".into(), detail: e.to_string() })?.version;
    let dir = patched_dir(repo_root, &id, platform);
    let path = dir.join(MANIFEST);
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(None) };
    let manifest: Manifest = serde_json::from_str(&text).map_err(|e| EngineError::Step { step: format!("reading {}", path.display()), detail: e.to_string() })?;
    if manifest.patchset != id || manifest.base != base {
        return Ok(None);
    }
    verify_build(&dir, &manifest, platform)?;
    // Where the host restricts user namespaces, only the copy `allow-sandbox` installed may start its sandbox.
    if crate::sandbox::userns_restricted() {
        let installed = crate::sandbox::installed_dir(&id, platform);
        let same = std::fs::read_to_string(installed.join(MANIFEST)).ok()
            .and_then(|text| serde_json::from_str::<Manifest>(&text).ok())
            .is_some_and(|copy| copy == manifest);
        if same && verify_build(&installed, &manifest, platform).is_ok() {
            return Ok(Some((installed.join(&manifest.binary), manifest)));
        }
    }
    Ok(Some((dir.join(&manifest.binary), manifest)))
}

/// Checks a build against its manifest: its platform, and every file it lists (the executable among them)
/// present with its digest. A file's digest is computed once per process for each length and modification
/// time it has: callers that ask for the same file at once wait for one computation, and a file reached
/// through a link is the file it resolves to.
fn verify_build(dir: &Path, manifest: &Manifest, platform: &str) -> Result<(), EngineError> {
    use std::sync::{Arc, Mutex, OnceLock};
    type Digest = Arc<Mutex<Option<(u64, std::time::SystemTime, String)>>>;
    static SEEN: OnceLock<Mutex<BTreeMap<PathBuf, Digest>>> = OnceLock::new();
    let corrupt = |detail: String| EngineError::Corrupt { dir: dir.display().to_string(), detail };
    if manifest.platform != platform {
        return Err(corrupt(format!("it is for {}, not {platform}", manifest.platform)));
    }
    if !manifest.files.contains_key(&manifest.binary) {
        return Err(corrupt(format!("{} is not among its files", manifest.binary)));
    }
    for (name, want) in &manifest.files {
        let path = dir.join(name);
        let meta = std::fs::metadata(&path).map_err(|e| corrupt(format!("{name}: {e}")))?;
        let key = (meta.len(), meta.modified().map_err(io(&path))?);
        let resolved = std::fs::canonicalize(&path).map_err(|e| corrupt(format!("{name}: {e}")))?;
        let seen = SEEN.get_or_init(Default::default).lock().unwrap().entry(resolved).or_default().clone();
        let mut seen = seen.lock().unwrap();
        let digest = match &*seen {
            Some((len, at, d)) if (*len, *at) == key => d.clone(),
            _ => {
                let d = sha256_hex(&path)?;
                *seen = Some((key.0, key.1, d.clone()));
                d
            }
        };
        if &digest != want {
            return Err(corrupt(format!("{name} has digest {digest}, not {want}")));
        }
    }
    Ok(())
}

/// Preserve build-input times after a normally exited step or a failure to start it.
/// Ninja retains dirty outputs; restoring identical input times must not make completed work dirty.
/// A killed build cannot establish that all of its workers have stopped, so leaves no new record.
fn run_build(src: &Path, step: &str, cmd: &mut Command) -> Result<(), EngineError> {
    // GN may have just recorded the inputs. Consume that record before starting
    // the compiler, so interruption cannot leave a certificate for a running job.
    match std::fs::remove_file(src.join(TREE)) {
        Ok(()) => {},
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(io(&src.join(TREE))(e)),
    }
    let status = cmd.stdout(std::process::Stdio::inherit()).stderr(std::process::Stdio::inherit()).status();
    // The initial GN generation has no Ninja log yet. A missing executable has
    // started no worker and is safe to record, just like a normal failed exit.
    let recorded = if !matches!(&status, Ok(s) if s.code().is_none()) && src.join(BUILD_LOG).is_file() { record_tree(src) } else { Ok(()) };
    if !status.as_ref().is_ok_and(|s| s.success()) {
        if let Err(error) = recorded {
            eprintln!("Could not preserve build input times: {error}");
        }
        let detail = match status { Ok(s) => s.to_string(), Err(e) => e.to_string() };
        return Err(EngineError::Step { step: step.into(), detail });
    }
    recorded
}

fn run(step: &str, cmd: &mut Command) -> Result<String, EngineError> {
    let out = cmd.output().map_err(|e| EngineError::Step { step: step.into(), detail: e.to_string() })?;
    if !out.status.success() {
        let tail: String = String::from_utf8_lossy(&out.stdout).lines().chain(String::from_utf8_lossy(&out.stderr).lines()).rev().take(30).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        return Err(EngineError::Step { step: step.into(), detail: format!("{}\n{tail}", out.status) });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The checkout's version, from `chrome/VERSION`.
pub fn checkout_version(src: &Path) -> Result<String, EngineError> {
    let path = src.join("chrome/VERSION");
    let text = std::fs::read_to_string(&path).map_err(io(&path))?;
    let field = |k: &str| text.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).unwrap_or("?").trim().to_string();
    Ok(format!("{}.{}.{}.{}", field("MAJOR"), field("MINOR"), field("BUILD"), field("PATCH")))
}

/// The marker of a build that has applied patches to the checkout (in the ignored `out/`): the digest of
/// the checkout's changes after each patch it applied. A build killed before it could revert leaves it,
/// and the next build reverts those changes only when they are exactly what it records — never edits
/// made since.
const APPLIED: &str = "out/frontcap/.frontcap-applied";
/// Held for a whole build: two builds never share a checkout.
const LOCK: &str = "out/frontcap/.frontcap-build.lock";

/// The digest of the checkout's changes to tracked files (staged and not) against its commit, as git's own
/// patch (no external diff tool, text conversion or color a configuration may set).
fn changes_digest(src: &Path) -> Result<String, EngineError> {
    let out = Command::new("git").arg("-C").arg(src).args(["diff", "--no-ext-diff", "--no-textconv", "--no-color", "--binary", "--full-index", "HEAD"]).output().map_err(|e| EngineError::Step { step: "git diff".into(), detail: e.to_string() })?;
    if !out.status.success() {
        return Err(EngineError::Step { step: "git diff".into(), detail: String::from_utf8_lossy(&out.stderr).into_owned() });
    }
    Ok(Sha256::digest(&out.stdout).iter().map(|b| format!("{b:02x}")).collect())
}

/// Records that the checkout's changes are this build's, as they stand.
fn mark_applied(src: &Path) -> Result<String, EngineError> {
    let digest = changes_digest(src)?;
    let path = src.join(APPLIED);
    std::fs::write(&path, &digest).map_err(io(&path))?;
    Ok(digest)
}

/// Puts the checkout back at its tag, on every way out of a build that unwinds, when its changes are still
/// exactly the ones the build made (`APPLIED`); otherwise it leaves them, and the marker, for the developer.
/// `git apply --index` stages what it adds, so a hard reset removes it too; nothing untracked is touched.
struct Revert(PathBuf);

impl Revert {
    fn ours(&self) -> bool {
        let recorded = std::fs::read_to_string(self.0.join(APPLIED)).ok();
        recorded.is_some_and(|r| changes_digest(&self.0).is_ok_and(|d| d == r.trim()))
    }
}

impl Drop for Revert {
    fn drop(&mut self) {
        if !self.ours() {
            eprintln!("the source checkout {} changed during the build; it is left as it is", self.0.display());
            return;
        }
        let reset = Command::new("git").arg("-C").arg(&self.0).args(["reset", "--hard", "--quiet"]).status();
        if reset.is_ok_and(|s| s.success()) {
            let _ = std::fs::remove_file(self.0.join(APPLIED));
        }
    }
}

struct AppliedCheckouts {
    // Restore source before releasing checkout locks, including on a build failure.
    _reverts: Vec<Revert>,
    _locks: Vec<std::fs::File>,
}

/// Check every repository before applying or recovering any patches. In particular,
/// a developer edit in V8 must not cause an owned Chromium patch to be discarded.
fn apply_patches(repo_root: &Path, src: &Path) -> Result<AppliedCheckouts, EngineError> {
    let mut checkouts = vec![(src.to_path_buf(), patches(repo_root)?)];
    let v8 = src.join("v8");
    let configured = v8_patches(repo_root)?;
    let v8_base = configured.as_ref().map(|(base, _)| base.clone());
    if configured.is_some() || v8.join(".git").exists() {
        let top = run("locating V8 checkout", Command::new("git").arg("-C").arg(&v8).args(["rev-parse", "--show-toplevel"]))?;
        let expected = std::fs::canonicalize(&v8).map_err(io(&v8))?;
        if Path::new(top.trim()) != expected {
            return Err(EngineError::Step { step: "locating V8 checkout".into(), detail: "src/v8 must be an independent git checkout".into() });
        }
        let patches = configured.map(|(_, patches)| patches).unwrap_or_default();
        checkouts.push((v8, patches));
    }
    let mut applied = AppliedCheckouts { _reverts: Vec::new(), _locks: Vec::new() };
    for (checkout, _) in &checkouts {
        std::fs::create_dir_all(checkout.join("out/frontcap")).map_err(io(checkout))?;
        let path = checkout.join(LOCK);
        let lock = std::fs::File::create(&path).map_err(io(&path))?;
        lock.lock().map_err(io(&path))?;
        applied._locks.push(lock);
    }
    if let Some(base) = v8_base {
        let v8 = src.join("v8");
        let found = run("reading V8 commit", Command::new("git").arg("-C").arg(&v8).args(["rev-parse", "HEAD"]))?;
        if found.trim() != base {
            return Err(EngineError::WrongBase { src: v8.display().to_string(), found: found.trim().into(), base });
        }
    }
    for (checkout, _) in &checkouts {
        let status = run("git status", Command::new("git").arg("-C").arg(checkout).args(["status", "--porcelain", "--untracked-files=no"]))?;
        if !status.trim().is_empty() {
            let path = checkout.join(APPLIED);
            let Ok(recorded) = std::fs::read_to_string(&path) else {
                return Err(EngineError::Dirty(checkout.display().to_string()));
            };
            if changes_digest(checkout)? != recorded.trim() {
                return Err(EngineError::Mixed(checkout.display().to_string()));
            }
        }
    }
    for (checkout, patches) in &checkouts {
        if checkout.join(APPLIED).is_file() {
            // Recheck ownership while restoring; never replace the recorded digest
            // with a developer edit made after the preflight.
            drop(Revert(checkout.clone()));
        }
        let status = run("git status", Command::new("git").arg("-C").arg(checkout).args(["status", "--porcelain", "--untracked-files=no"]))?;
        if !status.trim().is_empty() {
            return Err(EngineError::Mixed(checkout.display().to_string()));
        }
        mark_applied(checkout)?;
        applied._reverts.push(Revert(checkout.clone()));
        for patch in patches {
            let result = run(&format!("applying {}", patch.display()), Command::new("git").arg("-C").arg(checkout).args(["apply", "--index"]).arg(patch));
            mark_applied(checkout)?;
            result?;
        }
    }
    Ok(applied)
}

/// What the output directory was last built from (in the ignored `out/`): for each file that differed
/// from the checkout's commit, its content digest and modification time, and the build log as it stood
/// then. The build resets the checkout afterwards and patches it again next time, which gives unchanged
/// files new times; restoring the recorded time to a file whose content is what was built lets the next
/// build skip work that is still valid.
const TREE: &str = "out/frontcap/.frontcap-tree.json";
/// Every build appends to it: a different length or time means something was built since the record.
const BUILD_LOG: &str = "out/frontcap/.ninja_log";

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct BuiltFile {
    sha256: String,
    modified_ns: u128,
}

#[derive(Debug, Serialize, Deserialize)]
struct Tree {
    build_log: (u64, u128),
    files: BTreeMap<String, BuiltFile>,
}

fn nanos(t: std::time::SystemTime) -> u128 {
    t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

fn build_log_mark(src: &Path) -> Result<(u64, u128), EngineError> {
    let path = src.join(BUILD_LOG);
    let meta = std::fs::metadata(&path).map_err(io(&path))?;
    Ok((meta.len(), nanos(meta.modified().map_err(io(&path))?)))
}

/// The tracked files that differ from the checkout's commit (staged or not).
fn changed_files(src: &Path) -> Result<Vec<String>, EngineError> {
    let listed = run("git diff --name-only", Command::new("git").arg("-C").arg(src).args(["diff", "--no-ext-diff", "--name-only", "HEAD"]))?;
    Ok(listed.lines().filter(|l| !l.is_empty()).map(str::to_string).collect())
}

/// Include the independent V8 repository in input-time records. Chromium's git
/// diff cannot see it. Both repositories feed the same Ninja build log.
fn changed_build_inputs(src: &Path) -> Result<Vec<String>, EngineError> {
    let mut files = changed_files(src)?;
    if src.join("v8/.git").exists() {
        files.extend(changed_files(&src.join("v8"))?.into_iter().map(|p| format!("v8/{p}")));
    }
    Ok(files)
}

/// Records inputs after the builder exits, including a partial build. Unfinished outputs remain
/// dirty to Ninja. Run before checkout reset; `engine record-tree` supports in-tree builds.
pub fn record_tree(src: &Path) -> Result<(), EngineError> {
    let mut files = BTreeMap::new();
    for name in changed_build_inputs(src)? {
        let path = src.join(&name);
        if !path.is_file() {
            continue;
        }
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).map_err(io(&path))?;
        files.insert(name, BuiltFile { sha256: sha256_hex(&path)?, modified_ns: nanos(modified) });
    }
    let tree = Tree { build_log: build_log_mark(src)?, files };
    let path = src.join(TREE);
    std::fs::write(&path, serde_json::to_string(&tree).expect("serializes")).map_err(io(&path))
}

/// Gives each changed file whose content is what the output directory was built from the time it had
/// then, and returns how many it restored. Nothing is restored when anything was built since the record
/// (its objects may be of other content). The record is consumed either way, so a build interrupted after
/// this cannot leave it for a later one.
pub fn restore_tree(src: &Path) -> Result<usize, EngineError> {
    let path = src.join(TREE);
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(0) };
    std::fs::remove_file(&path).map_err(io(&path))?;
    let Ok(tree) = serde_json::from_str::<Tree>(&text) else { return Ok(0) };
    if build_log_mark(src).ok() != Some(tree.build_log) {
        return Ok(0);
    }
    let mut restored = 0;
    for name in changed_build_inputs(src)? {
        let Some(built) = tree.files.get(&name) else { continue };
        let file = src.join(&name);
        if sha256_hex(&file)? != built.sha256 {
            continue;
        }
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(built.modified_ns as u64);
        std::fs::File::options().write(true).open(&file).and_then(|f| f.set_modified(at)).map_err(io(&file))?;
        restored += 1;
    }
    Ok(restored)
}

/// The browser's runtime files, as the build declares them (`gn desc … runtime_deps`): paths relative
/// to the output directory, files only (a directory the list names is not needed by frontcap).
fn runtime_deps(src: &Path) -> Result<Vec<String>, EngineError> {
    let listed = run("gn desc runtime_deps", Command::new("gn").current_dir(src).args(["desc", "out/frontcap", "//headless:headless_shell", "runtime_deps"]))?;
    let out = src.join("out/frontcap");
    Ok(listed
        .lines()
        .map(|l| l.trim().trim_start_matches("./"))
        .filter(|l| !l.is_empty() && !l.starts_with("../") && !l.contains('/') && out.join(l).is_file())
        .map(str::to_string)
        .collect())
}

/// The GN arguments that build `platform` on `host`: none when they are the same platform (GN's defaults
/// are the host's), so a host's own build is configured by `args.gn` alone.
pub fn target_args(host: &str, platform: &str) -> Result<String, EngineError> {
    let target = |p: &str| match p {
        "mac-arm64" => Some(("mac", "arm64")),
        "mac-x64" => Some(("mac", "x64")),
        "linux64" => Some(("linux", "x64")),
        "linux-arm64" => Some(("linux", "arm64")),
        _ => None,
    };
    let (os, cpu) = target(platform).ok_or_else(|| EngineError::UnknownPlatform(platform.into()))?;
    if host == platform {
        return Ok(String::new());
    }
    Ok(format!("\n# The build platform, {platform}, from {host}.\ntarget_os = \"{os}\"\ntarget_cpu = \"{cpu}\"\n"))
}

/// Builds the patched engine for `platform` from a Chromium checkout at the pinned tag (depot_tools on
/// `PATH`) and keeps it under `browsers/patched/<patchset>/<platform>`. The checkout is left as it was
/// found. A platform other than the host's is a cross build, which the checkout must have been synced for.
pub fn build(repo_root: &Path, src: &Path, base: &str, platform: &str) -> Result<PathBuf, EngineError> {
    let host = crate::current_platform().map_err(|e| EngineError::Step { step: "reading the host platform".into(), detail: e.to_string() })?;
    let target = target_args(host, platform)?;
    let found = checkout_version(src)?;
    if found != base {
        return Err(EngineError::WrongBase { src: src.display().to_string(), found, base: base.into() });
    }
    let id = patchset_id(repo_root)?;
    let _applied = apply_patches(repo_root, src)?;
    // Files the last build compiled as they are now keep that build's times (`TREE`).
    let unchanged = restore_tree(src)?;
    if unchanged > 0 {
        eprintln!("{unchanged} patched files are unchanged since the last build of this checkout");
    }
    let args_path = engine_dir(repo_root).join("args.gn");
    let args = std::fs::read_to_string(&args_path).map_err(io(&args_path))? + &target;
    let out = src.join("out/frontcap");
    std::fs::create_dir_all(&out).map_err(io(&out))?;
    std::fs::write(out.join("args.gn"), &args).map_err(io(&out))?;
    run_build(src, "gn gen", Command::new("gn").current_dir(src).args(["gen", "out/frontcap"]))?;
    let mut autoninja = Command::new("autoninja");
    autoninja.current_dir(src).args(["-C", "out/frontcap", "headless_shell"]);
    // Parallel compiles beyond the host's memory get the build killed; the default is the build tool's.
    if let Ok(jobs) = std::env::var("FRONTCAP_BUILD_JOBS") {
        autoninja.args(["-j", &jobs]);
    }
    run_build(src, "autoninja", &mut autoninja)?;
    let deps = runtime_deps(src)?;

    let dest = patched_dir(repo_root, &id, platform);
    let parent = dest.parent().expect("has a parent");
    std::fs::create_dir_all(parent).map_err(io(parent))?;
    let staging = parent.join(format!(".{platform}.partial-{}", std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(io(&staging))?;
    }
    std::fs::create_dir_all(&staging).map_err(io(&staging))?;
    let kept = (|| {
        let mut files = BTreeMap::new();
        for name in &deps {
            let to = staging.join(name);
            std::fs::copy(out.join(name), &to).map_err(io(&to))?;
            files.insert(name.clone(), sha256_hex(&to)?);
        }
        if !files.contains_key("headless_shell") {
            return Err(EngineError::Step { step: "packaging".into(), detail: format!("{} has no headless_shell", out.display()) });
        }
        let manifest = Manifest { base: base.into(), patchset: id.clone(), platform: platform.into(), binary: "headless_shell".into(), files };
        let path = staging.join(MANIFEST);
        std::fs::write(&path, serde_json::to_string_pretty(&manifest).expect("serializes") + "\n").map_err(io(&path))
    })();
    if let Err(e) = kept {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    // The earlier build is moved aside before the new one moves in, so a launch never finds half of one.
    let old = parent.join(format!(".{platform}.old-{}", std::process::id()));
    if dest.exists() {
        std::fs::rename(&dest, &old).map_err(io(&dest))?;
    }
    std::fs::rename(&staging, &dest).map_err(io(&dest))?;
    let _ = std::fs::remove_dir_all(&old);
    // Leftovers of killed builds.
    if let Ok(entries) = std::fs::read_dir(parent) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(&format!(".{platform}.")) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    Ok(dest.join("headless_shell"))
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
