//! Shared per-account native target exclusion and durable unretired-target fence.
use frontcap_core::state::load;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostSession {
    pub directory: PathBuf,
    pub retired: bool,
}
/// A normal release must not wait for unrelated forked children to exec or exit.
/// The durable HostSession fence remains independent of this process-held lease.
pub(crate) struct HostLease {
    file: File,
    owner_pid: u32,
}
impl Drop for HostLease {
    fn drop(&mut self) {
        // A forked child must not unlock the parent's shared file description.
        if std::process::id() == self.owner_pid {
            let _ = self.file.unlock();
        }
    }
}

pub(crate) fn acquire(host: &Path) -> io::Result<HostLease> {
    std::fs::create_dir_all(host)?;
    let lease = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(host.join("native-tests.lock"))?;
    lease
        .try_lock()
        .map_err(|e| io::Error::other(format!("native host already owned: {e}")))?;
    let lease = HostLease { file: lease, owner_pid: std::process::id() };
    if let Some(prior) = load::<HostSession>(&host.join("native-session.json"))? {
        if !prior.directory.is_absolute() {
            return Err(io::Error::other("invalid native host session directory"));
        }
        if !prior.retired {
            return Err(io::Error::other(format!(
                "previous native session is unretired; inspect {}",
                prior.directory.display()
            )));
        }
    }
    Ok(lease)
}
