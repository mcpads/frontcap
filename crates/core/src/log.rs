use crate::canonical::canonical_of;
use crate::event::{AcquisitionEvent, AcquisitionKind, BehaviorEvent, EventKind};
use crate::time::VirtualTime;
use crate::lines::{Appender, LinesError, open_append, read_complete, read_complete_from};
use std::fs::File;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error(transparent)]
    Lines(#[from] LinesError),
    #[error("io error at {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("cannot parse line {line} of {path}: {source}")]
    Parse { path: String, line: usize, source: serde_json::Error },
    #[error("virtual time went backwards: last {last:?}, attempted {attempted:?}")]
    TimeWentBackwards { last: VirtualTime, attempted: VirtualTime },
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> LogError + '_ {
    move |source| LogError::Io { path: path.display().to_string(), source }
}

fn parse_lines<T: serde::de::DeserializeOwned>(path: &Path, lines: &[String]) -> Result<Vec<T>, LogError> {
    parse_lines_after(path, 0, lines)
}

/// Parses `lines`, which follow `before` lines of the file.
fn parse_lines_after<T: serde::de::DeserializeOwned>(path: &Path, before: usize, lines: &[String]) -> Result<Vec<T>, LogError> {
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str(l).map_err(|source| LogError::Parse { path: path.display().to_string(), line: before + i + 1, source })
        })
        .collect()
}

pub struct BehaviorLog {
    path: PathBuf,
    out: Appender<File>,
    next_seq: u64,
    last_time: VirtualTime,
    recovered: Option<PathBuf>,
}

impl BehaviorLog {
    pub fn open(path: &Path) -> Result<BehaviorLog, LogError> {
        let opened = open_append(path)?;
        let events: Vec<BehaviorEvent> = parse_lines(path, &opened.lines)?;
        let (next_seq, last_time) = events.last().map(|e| (e.seq + 1, e.t_us)).unwrap_or((0, VirtualTime::ZERO));
        Ok(BehaviorLog {
            path: path.to_path_buf(),
            out: Appender::new(opened.file, opened.len),
            next_seq,
            last_time,
            recovered: opened.recovered,
        })
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub fn last_time(&self) -> VirtualTime {
        self.last_time
    }

    pub fn recovered_tail(&self) -> Option<&Path> {
        self.recovered.as_deref()
    }

    pub fn append(&mut self, t: VirtualTime, kind: EventKind) -> Result<u64, LogError> {
        if t < self.last_time {
            return Err(LogError::TimeWentBackwards { last: self.last_time, attempted: t });
        }
        let seq = self.next_seq;
        let line = canonical_of(&BehaviorEvent { seq, t_us: t, kind });
        self.out.append_line(&line).map_err(io(&self.path))?;
        self.next_seq += 1;
        self.last_time = t;
        Ok(seq)
    }

    pub fn sync(&mut self) -> Result<(), LogError> {
        self.out.sync().map_err(io(&self.path))
    }

    /// Complete events of a log; a partial tail from an interrupted write is ignored.
    pub fn read(path: &Path) -> Result<Vec<BehaviorEvent>, LogError> {
        parse_lines(path, &read_complete(path)?)
    }
}

/// The complete events of one behavior log, kept between reads so that each read parses only the lines
/// appended since the last one. A read returns what [`BehaviorLog::read`] returns at that moment.
///
/// The log is append-only and its sequence numbers are consecutive. A file that is not the one read
/// before (another file at the path, a shorter one, or a line that does not continue the sequence) is
/// read again from its start.
pub struct BehaviorTail {
    path: PathBuf,
    file: Option<FileIdentity>,
    offset: u64,
    events: Vec<BehaviorEvent>,
}

#[cfg(unix)]
type FileIdentity = (u64, u64);
#[cfg(not(unix))]
type FileIdentity = ();

fn file_identity(metadata: &std::fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    let _ = metadata;
}

impl BehaviorTail {
    pub fn new(path: &Path) -> BehaviorTail {
        BehaviorTail { path: path.to_path_buf(), file: None, offset: 0, events: Vec::new() }
    }

    /// The process's one tail of the log at `path`, so that its readers parse and keep each line once.
    /// It lasts while a reader holds it.
    pub fn shared(path: &Path) -> std::sync::Arc<std::sync::Mutex<BehaviorTail>> {
        use std::sync::{Arc, Mutex, Weak};
        static TAILS: Mutex<Vec<(PathBuf, Weak<Mutex<BehaviorTail>>)>> = Mutex::new(Vec::new());
        let mut tails = TAILS.lock().unwrap();
        tails.retain(|(_, tail)| tail.strong_count() > 0);
        if let Some(tail) = tails.iter().find(|(of, _)| of == path).and_then(|(_, tail)| tail.upgrade()) {
            return tail;
        }
        let tail = Arc::new(Mutex::new(BehaviorTail::new(path)));
        tails.push((path.to_path_buf(), Arc::downgrade(&tail)));
        tail
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Complete events of the log; a partial tail from an interrupted or unfinished write is ignored.
    pub fn events(&mut self) -> Result<&[BehaviorEvent], LogError> {
        let mut file = File::open(&self.path).map_err(io(&self.path))?;
        let metadata = file.metadata().map_err(io(&self.path))?;
        let identity = file_identity(&metadata);
        if self.file != Some(identity) || metadata.len() < self.offset {
            self.offset = 0;
            self.events.clear();
        }
        self.file = Some(identity);
        loop {
            let before = self.events.len();
            let (read, lines) = read_complete_from(&mut file, &self.path, self.offset).map_err(|e| match e {
                LinesError::Corrupt { path, line } => LinesError::Corrupt { path, line: before + line },
                e => e,
            })?;
            let appended: Vec<BehaviorEvent> = parse_lines_after(&self.path, before, &lines)?;
            let continues = match (self.events.last(), appended.first()) {
                (Some(last), Some(first)) => first.seq == last.seq + 1,
                _ => true,
            };
            if !continues {
                self.offset = 0;
                self.events.clear();
                continue;
            }
            self.offset += read;
            self.events.extend(appended);
            return Ok(&self.events);
        }
    }
}

pub struct AcquisitionLog {
    path: PathBuf,
    out: Appender<File>,
    next_aseq: u64,
    recovered: Option<PathBuf>,
}

impl AcquisitionLog {
    pub fn open(path: &Path) -> Result<AcquisitionLog, LogError> {
        let opened = open_append(path)?;
        let events: Vec<AcquisitionEvent> = parse_lines(path, &opened.lines)?;
        let next_aseq = events.last().map(|e| e.aseq + 1).unwrap_or(0);
        Ok(AcquisitionLog {
            path: path.to_path_buf(),
            out: Appender::new(opened.file, opened.len),
            next_aseq,
            recovered: opened.recovered,
        })
    }

    pub fn recovered_tail(&self) -> Option<&Path> {
        self.recovered.as_deref()
    }

    pub fn append(&mut self, after_seq: u64, t: VirtualTime, kind: AcquisitionKind) -> Result<u64, LogError> {
        let aseq = self.next_aseq;
        let line = canonical_of(&AcquisitionEvent { aseq, after_seq, t_us: t, kind });
        self.out.append_line(&line).map_err(io(&self.path))?;
        self.next_aseq += 1;
        Ok(aseq)
    }

    pub fn sync(&mut self) -> Result<(), LogError> {
        self.out.sync().map_err(io(&self.path))
    }

    pub fn read(path: &Path) -> Result<Vec<AcquisitionEvent>, LogError> {
        parse_lines(path, &read_complete(path)?)
    }
}

#[cfg(test)]
#[path = "log_tests.rs"]
mod tests;
