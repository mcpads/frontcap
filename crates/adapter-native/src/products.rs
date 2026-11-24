//! Freeze XCTest build products before a session acquires or launches a target.
use frontcap_core::{native::request::InvalidRequest, write_budget::{Class, WriteBudget}};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Component, Path},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductFile {
    pub bytes: u64,
    pub sha256: String,
    pub executable: bool,
}
/// Complete app/directory contents, independent of a consumer framework.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleInventory {
    pub files: BTreeMap<String, ProductFile>,
    pub directories: BTreeSet<String>,
}
impl BundleInventory {
    pub fn capture(root: &Path) -> io::Result<Self> {
        Self::capture_checked(root, &mut || Ok(()))
    }
    pub(crate) fn capture_checked(root: &Path, check: &mut dyn FnMut() -> io::Result<()>) -> io::Result<Self> {
        let (directories, paths) = inventory_checked(root, check)?;
        let files = paths.into_iter().map(|name| Ok((name.clone(), read_file_checked(&root.join(name), None, check)?)))
            .collect::<io::Result<_>>()?;
        check()?;
        Ok(Self { files, directories })
    }
    pub fn validate(&self) -> io::Result<()> {
        for name in self.directories.iter().chain(self.files.keys()) { relative(name)?; }
        for (name, file) in &self.files {
            if self.directories.contains(name) || file.sha256.len() != 64
                || !file.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
                return Err(io::Error::other("invalid frozen bundle inventory"));
            }
        }
        Ok(())
    }
    pub fn verify(&self, root: &Path) -> io::Result<()> {
        self.verify_checked(root, &mut || Ok(()))
    }
    fn verify_checked(&self, root: &Path, check: &mut dyn FnMut() -> io::Result<()>) -> io::Result<()> {
        check()?;
        self.validate()?;
        let observed = Self::capture_checked(root, check)?;
        if observed != *self {
            let difference = self.files.iter().find_map(|(path, expected)| match observed.files.get(path) {
                None => Some(("missing file", path)),
                Some(actual) if actual != expected => Some(("changed file", path)),
                _ => None,
            }).or_else(|| observed.files.keys().find(|path| !self.files.contains_key(*path))
                .map(|path| ("unexpected file", path)))
              .or_else(|| self.directories.symmetric_difference(&observed.directories).next()
                .map(|path| ("changed directory", path)));
            let (kind, path) = difference.expect("unequal inventories have a difference");
            let sample: String = path.chars().take(128).collect();
            return Err(io::Error::other(format!("bundle differs from frozen inventory: {kind} {sample:?}")));
        }
        Ok(())
    }
    /// Copy declared data into a new directory, preserving failed copies as incomplete evidence.
    pub fn copy_verified(&self, source: &Path, destination: &Path) -> io::Result<()> {
        self.copy_verified_checked(source, destination, &mut || Ok(()))
    }
    /// Cooperatively checks authority during traversal, hashing, copying and synchronization.
    /// An individual filesystem call cannot be interrupted by this callback.
    pub(crate) fn copy_verified_checked(&self, source: &Path, destination: &Path,
        check: &mut dyn FnMut() -> io::Result<()>) -> io::Result<()> {
        self.verify_checked(source, check)?;
        copy_files_checked(source, destination, &self.directories, &self.files, check)?;
        self.verify_checked(destination, check)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductsManifest {
    #[serde(default)]
    pub role: ProductRole,
    pub xctestrun: String,
    pub names: ProductNames,
    pub directories: BTreeSet<String>,
    pub files: BTreeMap<String, ProductFile>,
}
/// The external runner does not require instrumentation in its placeholder app.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductRole {
    #[default]
    ClockApp,
    ExternalRunner,
}
/// Build-product names are frozen alongside their complete byte inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductNames {
    pub app: String,
    pub tests: String,
}
#[cfg(test)]
impl Default for ProductNames {
    fn default() -> Self {
        Self {
            app: "SampleApp".into(),
            tests: "SampleAppTests".into(),
        }
    }
}
impl ProductNames {
    fn required(&self) -> Result<Vec<String>, InvalidRequest> {
        for name in [&self.app, &self.tests] {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            {
                return Err(InvalidRequest("invalid XCTest product name"));
            }
        }
        let app = format!("Debug-iphonesimulator/{}.app", self.app);
        let runner = format!("Debug-iphonesimulator/{}-Runner.app", self.tests);
        let tests = format!("{runner}/PlugIns/{}.xctest", self.tests);
        Ok(vec![
            format!("{app}/Info.plist"),
            format!("{app}/{}", self.app),
            format!("{app}/vclock.dylib"),
            format!("{app}/ui_schedule.dylib"),
            format!("{runner}/Info.plist"),
            format!("{runner}/{}-Runner", self.tests),
            format!("{tests}/Info.plist"),
            format!("{tests}/{}", self.tests),
        ])
    }
}
#[cfg(test)]
const REQUIRED: &[&str] = &[
    "Debug-iphonesimulator/SampleApp.app/Info.plist",
    "Debug-iphonesimulator/SampleApp.app/SampleApp",
    "Debug-iphonesimulator/SampleApp.app/vclock.dylib",
    "Debug-iphonesimulator/SampleApp.app/ui_schedule.dylib",
    "Debug-iphonesimulator/SampleAppTests-Runner.app/Info.plist",
    "Debug-iphonesimulator/SampleAppTests-Runner.app/SampleAppTests-Runner",
    "Debug-iphonesimulator/SampleAppTests-Runner.app/PlugIns/SampleAppTests.xctest/Info.plist",
    "Debug-iphonesimulator/SampleAppTests-Runner.app/PlugIns/SampleAppTests.xctest/SampleAppTests",
];
fn relative(path: &str) -> io::Result<()> {
    if path.is_empty()
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(io::Error::other(
            "product path is not a relative descendant",
        ));
    }
    Ok(())
}
pub(crate) fn inventory(root: &Path) -> io::Result<(BTreeSet<String>, BTreeSet<String>)> {
    inventory_checked(root, &mut || Ok(()))
}
fn inventory_checked(root: &Path, check: &mut dyn FnMut() -> io::Result<()>) -> io::Result<(BTreeSet<String>, BTreeSet<String>)> {
    check()?;
    if !std::fs::symlink_metadata(root)?.is_dir() {
        return Err(io::Error::other("product root is not a directory"));
    }
    fn visit(
        root: &Path,
        at: &Path,
        dirs: &mut BTreeSet<String>,
        files: &mut BTreeSet<String>,
        check: &mut dyn FnMut() -> io::Result<()>,
    ) -> io::Result<()> {
        for entry in std::fs::read_dir(at)? {
            check()?;
            let path = entry?.path();
            let name = path
                .strip_prefix(root)
                .map_err(io::Error::other)?
                .to_str()
                .ok_or_else(|| io::Error::other("non-UTF8 product path"))?
                .to_owned();
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                dirs.insert(name);
                visit(root, &path, dirs, files, check)?;
            } else if meta.is_file() {
                files.insert(name);
            } else {
                return Err(io::Error::other(
                    "product tree contains a link or special file",
                ));
            }
        }
        Ok(())
    }
    let mut dirs = BTreeSet::new();
    let mut files = BTreeSet::new();
    visit(root, root, &mut dirs, &mut files, check)?;
    Ok((dirs, files))
}
pub(crate) fn read_file(path: &Path, output: Option<&mut File>) -> io::Result<ProductFile> {
    read_file_checked(path, output, &mut || Ok(()))
}
fn read_file_checked(path: &Path, output: Option<&mut File>, check: &mut dyn FnMut() -> io::Result<()>) -> io::Result<ProductFile> {
    read_file_limited(path, output, check, None)
}
pub(crate) fn read_file_limited(path: &Path, mut output: Option<&mut File>, check: &mut dyn FnMut() -> io::Result<()>, limit: Option<u64>) -> io::Result<ProductFile> {
    check()?;
    // Reject a substituted symlink at open as well as during inventory; copy only regular files.
    let mut options = OpenOptions::new();
    options.read(true);
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW);
    let mut input = options.open(path)?;
    let meta = input.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("product is not a regular file"));
    }
    if limit.is_some_and(|limit| meta.len() != limit) {
        return Err(io::Error::other("product size differs from admitted payload"));
    }
    let mut hash = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 65536];
    loop {
        check()?;
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        let next = bytes.checked_add(n as u64).ok_or_else(|| io::Error::other("product size overflow"))?;
        if limit.is_some_and(|limit| next > limit) {
            return Err(io::Error::other("product grew beyond admitted payload"));
        }
        hash.update(&buffer[..n]);
        bytes = next;
        if let Some(file) = output.as_mut() {
            file.write_all(&buffer[..n])?;
        }
    }
    if bytes != meta.len() {
        return Err(io::Error::other("product size changed while reading"));
    }
    Ok(ProductFile {
        bytes,
        sha256: hash.finalize().iter().map(|b| format!("{b:02x}")).collect(),
        executable: meta.permissions().mode() & 0o111 != 0,
    })
}
impl ProductsManifest {
    #[cfg(test)]
    pub fn capture(root: &Path, xctestrun: &str) -> io::Result<Self> {
        Self::capture_named(root, xctestrun, ProductNames::default())
    }
    pub fn capture_named(root: &Path, xctestrun: &str, names: ProductNames) -> io::Result<Self> {
        Self::capture_role(root, xctestrun, names, ProductRole::ClockApp)
    }
    pub fn capture_role(
        root: &Path,
        xctestrun: &str,
        names: ProductNames,
        role: ProductRole,
    ) -> io::Result<Self> {
        names.required().map_err(io::Error::other)?;
        relative(xctestrun)?;
        let (directories, paths) = inventory(root)?;
        let mut files = BTreeMap::new();
        for path in paths {
            files.insert(path.clone(), read_file(&root.join(&path), None)?);
        }
        let manifest = Self {
            role,
            xctestrun: xctestrun.into(),
            names,
            directories,
            files,
        };
        manifest.validate().map_err(io::Error::other)?;
        Ok(manifest)
    }
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        for path in self
            .directories
            .iter()
            .chain(self.files.keys())
            .chain(std::iter::once(&self.xctestrun))
        {
            relative(path).map_err(|_| InvalidRequest("invalid frozen product path"))?;
            let mut parent = Path::new(path).parent();
            while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
                if !self.directories.contains(p.to_str().unwrap()) {
                    return Err(InvalidRequest("missing product parent directory"));
                }
                parent = p.parent();
            }
        }
        if !self.xctestrun.ends_with(".xctestrun")
            || !self.files.contains_key(&self.xctestrun)
            || self.directories.iter().any(|p| self.files.contains_key(p))
        {
            return Err(InvalidRequest("invalid XCTest product inventory"));
        }
        for path in self.names.required()? {
            if self.role == ProductRole::ExternalRunner
                && (path.ends_with("/vclock.dylib") || path.ends_with("/ui_schedule.dylib"))
            {
                continue;
            }
            let file = self
                .files
                .get(&path)
                .ok_or(InvalidRequest("missing required XCTest product"))?;
            if file.bytes == 0 || (!path.ends_with("Info.plist") && !file.executable) {
                return Err(InvalidRequest(
                    "required XCTest product is empty or not executable",
                ));
            }
        }
        for file in self.files.values() {
            if file.sha256.len() != 64
                || !file
                    .sha256
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            {
                return Err(InvalidRequest("invalid product hash"));
            }
        }
        Ok(())
    }
    pub fn verify(&self, root: &Path) -> io::Result<()> {
        self.validate().map_err(io::Error::other)?;
        let (dirs, files) = inventory(root)?;
        if dirs != self.directories || files != self.files.keys().cloned().collect() {
            return Err(io::Error::other(
                "product inventory differs from frozen inputs",
            ));
        }
        for (name, expected) in &self.files {
            if &read_file(&root.join(name), None)? != expected {
                return Err(io::Error::other(format!(
                    "product differs from frozen input: {name}"
                )));
            }
        }
        Ok(())
    }
    /// Destination must be new. Failed copies remain evidence and never qualify as runnable inputs.
    pub fn copy_verified(&self, source: &Path, destination: &Path) -> io::Result<()> {
        self.verify(source)?;
        copy_files(source, destination, &self.directories, &self.files)?;
        self.verify(destination)
    }
}

fn copy_files(source: &Path, destination: &Path, directories: &BTreeSet<String>,
    files: &BTreeMap<String, ProductFile>) -> io::Result<()> {
    copy_files_checked(source, destination, directories, files, &mut || Ok(()))
}
fn copy_files_checked(source: &Path, destination: &Path, directories: &BTreeSet<String>,
    files: &BTreeMap<String, ProductFile>, check: &mut dyn FnMut() -> io::Result<()>) -> io::Result<()> {
    let budget = WriteBudget::from_environment()?;
    copy_files_admitted(source, destination, directories, files, check, budget.as_ref())
}
fn copy_files_admitted(source: &Path, destination: &Path, directories: &BTreeSet<String>,
    files: &BTreeMap<String, ProductFile>, check: &mut dyn FnMut() -> io::Result<()>,
    budget: Option<&WriteBudget>) -> io::Result<()> {
    check()?;
    match destination.symlink_metadata() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
        Ok(_) => return Err(io::Error::other("product copy destination already exists")),
    }
    let bytes = files.values().try_fold(0_u64, |total, file| total.checked_add(file.bytes)
        .ok_or_else(|| io::Error::other("product inventory size overflow")))?;
    if let Some(budget) = budget { budget.charge(Class::Ordinary, bytes)?; }
    std::fs::create_dir(destination)?;
    for name in directories {
        check()?;
        std::fs::create_dir(destination.join(name))?;
    }
    for (name, expected) in files {
        check()?;
        let path = destination.join(name);
        let mut output = File::create_new(&path)?;
        if &read_file_limited(&source.join(name), Some(&mut output), check, Some(expected.bytes))? != expected {
            return Err(io::Error::other(format!(
                "product changed during copy: {name}"
            )));
        }
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(if expected.executable { 0o700 } else { 0o600 }),
        )?;
        check()?;
        output.sync_all()?;
    }
    for name in directories.iter().rev() {
        check()?;
        File::open(destination.join(name))?.sync_all()?;
    }
    check()?;
    File::open(destination)?.sync_all()?;
    check()?;
    Ok(())
}

#[cfg(test)]
#[path = "products_tests.rs"]
pub(crate) mod tests;
