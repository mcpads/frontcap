//! The one routine every append-only JSONL file in Core uses to open, read, and append.
//!
//! Only newline-terminated lines are complete. On open, bytes after the last newline are a partial line
//! from an interrupted write: they are copied to `<file>.partial-<n>`, synced, and truncated from the
//! file. A complete line that is not valid UTF-8 is corruption, not a partial write. A newly created file
//! has its directory synced so the file itself survives a power loss.
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum LinesError {
    #[error("io error at {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("line {line} of {path} is not valid UTF-8")]
    Corrupt { path: String, line: usize },
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> LinesError + '_ {
    move |source| LinesError::Io { path: path.display().to_string(), source }
}

fn sync_parent(path: &Path) -> Result<(), LinesError> {
    #[cfg(unix)]
    {
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        File::open(dir).and_then(|d| d.sync_all()).map_err(io(dir))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn split_complete(path: &Path, bytes: &[u8]) -> Result<(usize, Vec<String>), LinesError> {
    let cut = bytes.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
    let mut lines = Vec::new();
    for (i, raw) in bytes[..cut].split(|b| *b == b'\n').enumerate() {
        if raw.is_empty() {
            continue;
        }
        let text = std::str::from_utf8(raw)
            .map_err(|_| LinesError::Corrupt { path: path.display().to_string(), line: i + 1 })?;
        lines.push(text.to_string());
    }
    Ok((cut, lines))
}

pub struct Opened {
    pub file: File,
    pub lines: Vec<String>,
    pub recovered: Option<PathBuf>,
    /// Length of the file after recovery, i.e. the end of the last complete line.
    pub len: u64,
}

pub fn open_append(path: &Path) -> Result<Opened, LinesError> {
    let (bytes, existed) = match std::fs::read(path) {
        Ok(b) => (b, true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), false),
        Err(e) => return Err(io(path)(e)),
    };
    let (cut, lines) = split_complete(path, &bytes)?;
    let mut recovered = None;
    if cut < bytes.len() {
        let mut n = 0;
        let aside = loop {
            let candidate = PathBuf::from(format!("{}.partial-{n}", path.display()));
            if !candidate.exists() {
                break candidate;
            }
            n += 1;
        };
        {
            let mut f = File::create(&aside).map_err(io(&aside))?;
            f.write_all(&bytes[cut..]).map_err(io(&aside))?;
            f.sync_all().map_err(io(&aside))?;
        }
        sync_parent(&aside)?;
        let f = OpenOptions::new().write(true).open(path).map_err(io(path))?;
        f.set_len(cut as u64).map_err(io(path))?;
        f.sync_all().map_err(io(path))?;
        recovered = Some(aside);
    }
    let file = OpenOptions::new().create(true).append(true).open(path).map_err(io(path))?;
    if !existed {
        sync_parent(path)?;
    }
    Ok(Opened { file, lines, recovered, len: cut as u64 })
}

/// Complete lines of a file, ignoring a partial tail; does not modify the file.
pub fn read_complete(path: &Path) -> Result<Vec<String>, LinesError> {
    let bytes = std::fs::read(path).map_err(io(path))?;
    Ok(split_complete(path, &bytes)?.1)
}

/// Complete lines of `file` from `offset`, which ends a complete line, and the bytes they span. A partial
/// tail is left for a later read; the file is not modified.
pub fn read_complete_from(file: &mut File, path: &Path, offset: u64) -> Result<(u64, Vec<String>), LinesError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(offset)).and_then(|_| file.read_to_end(&mut bytes)).map_err(io(path))?;
    let (cut, lines) = split_complete(path, &bytes)?;
    Ok((cut as u64, lines))
}

/// Where an [`Appender`] writes. `File` is the production sink; tests inject failures.
pub trait Sink {
    fn write_all_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    fn truncate_to(&mut self, len: u64) -> std::io::Result<()>;
    fn sync(&mut self) -> std::io::Result<()>;
}

impl Sink for File {
    fn write_all_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.write_all(bytes)
    }

    fn truncate_to(&mut self, len: u64) -> std::io::Result<()> {
        self.set_len(len)
    }

    fn sync(&mut self) -> std::io::Result<()> {
        self.sync_data()
    }
}

/// Appends whole lines. A failed write is truncated back to the last complete line and the appender
/// refuses further writes, so a retry can never be glued onto partial bytes.
pub struct Appender<S: Sink> {
    sink: S,
    len: u64,
    poisoned: bool,
}

impl<S: Sink> Appender<S> {
    pub fn new(sink: S, len: u64) -> Appender<S> {
        Appender { sink, len, poisoned: false }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn append_line(&mut self, line: &str) -> std::io::Result<()> {
        if self.poisoned {
            return Err(std::io::Error::other("appender is unusable after a failed write"));
        }
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        if let Err(e) = self.sink.write_all_bytes(&bytes) {
            self.poisoned = true;
            let _ = self.sink.truncate_to(self.len);
            return Err(e);
        }
        self.len += bytes.len() as u64;
        Ok(())
    }

    pub fn sync(&mut self) -> std::io::Result<()> {
        self.sink.sync()
    }
}

#[cfg(test)]
#[path = "lines_test_support.rs"]
pub mod tests_support;
