//! Live child handles shared by native targets; saved process identities grant no signal authority.
use crate::{process::OwnedChild, resource_group::{Group, Operation, Reply}};
use frontcap_core::{ids::RequestId, process::ProcessIdentity};
use std::{io, path::PathBuf, sync::{Arc, Mutex}, time::{Duration, Instant}};

/// Trusted private executable entry points, with an absolute lifetime including acquisition.
pub struct Supervision {
    pub executable: PathBuf,
    pub group_arguments: Vec<String>,
    pub resource_arguments: Vec<String>,
    pub deadline_ns: u64,
}
pub(crate) type Shared = Arc<Mutex<Option<Group>>>;
pub(crate) fn with_group<T>(shared: &Shared, work: impl FnOnce(&mut Group) -> io::Result<T>) -> io::Result<T> {
    let mut guard = shared.lock().map_err(|_| io::Error::other("resource group lock poisoned"))?;
    work(guard.as_mut().ok_or_else(|| io::Error::other("resource group already retired"))?)
}
pub(crate) enum Process {
    Direct(OwnedChild),
    Supervised { group: Shared, id: RequestId, identity: ProcessIdentity,
        result: Option<Box<crate::resource_process::ResultRecord>> },
}
impl Process {
    pub fn id(&self) -> u32 { match self { Self::Direct(child) => child.id(), Self::Supervised { identity, .. } => identity.pid } }
    pub fn exited(&mut self) -> io::Result<bool> { match self {
        Self::Direct(child) => child.exited(),
        Self::Supervised { identity, .. } => identity.has_exited(),
    } }
    pub fn retire(&mut self, exit_code: &mut Option<i32>, signal_error: &mut Option<String>) -> io::Result<()> {
        match self {
            Self::Direct(child) => child.retire(exit_code, signal_error),
            Self::Supervised { group, id, result, .. } => {
                if result.is_none() {
                    let reply = with_group(group, |group| group.execute(Operation::Stop { id: id.clone() }, Instant::now() + Duration::from_secs(10)))?;
                    let Reply::Stopped { result: stopped } = reply else { return Err(io::Error::other(format!("resource stop unresolved: {reply:?}"))); };
                    *result = Some(Box::new(stopped));
                }
                let result = result.as_ref().unwrap();
                *exit_code = result.exit_code; *signal_error = result.signal_error.clone();
                match &result.retirement_error { Some(error) => Err(io::Error::other(error.clone())), None => Ok(()) }
            }
        }
    }
}
