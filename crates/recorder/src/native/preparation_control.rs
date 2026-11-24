//! Acquisition-only admission; the service journal owns release identity across phase changes.
use super::preparation;
use crate::runtime::{self, RuntimeState};
use frontcap_core::ids::{LaunchId, RequestId};
use frontcap_recorder_protocol::frame::{read_frame, write_frame};
use frontcap_service_fixture::{Control, Reply, owned::Controller};
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
    sync::atomic::{AtomicBool, Ordering},
};
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
    task::JoinHandle,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub launch_id: LaunchId,
    pub generation: LaunchId,
    pub release_id: RequestId,
    pub request: RequestId,
    pub sha256: String,
}
impl Release {
    fn command(&self) -> Control {
        Control::ReleaseAsset {
            generation: self.generation.clone(),
            id: self.release_id.clone(),
            request: self.request.clone(),
            sha256: self.sha256.clone(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cancel {
    pub launch_id: LaunchId,
    pub request_id: RequestId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Cancellation {
    /// Original owner accepted the signal; this does not prove target retirement.
    Requested { request: Cancel },
    NotSent { reason: String },
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Request { Release(Release), Cancel(Cancel) }
fn cancellation_path(directory: &Path) -> PathBuf { directory.join("preparation-cancel.json") }
fn socket(directory: &Path) -> PathBuf {
    runtime::socket_path(&directory.join("preparation-control"))
}
pub(super) struct Endpoint {
    listener: UnixListener,
    path: PathBuf,
    directory: PathBuf,
}
impl Endpoint {
    pub async fn bind(directory: &Path) -> io::Result<Self> {
        std::fs::create_dir(directory.join("preparation-control"))?;
        let path = socket(directory);
        let listener = UnixListener::bind(&path)?;
        Ok(Self { listener, path, directory: directory.to_owned() })
    }
    pub async fn acquire<T>(
        &self,
        mut task: JoinHandle<io::Result<T>>,
        controller: Option<Controller>,
        launch: &LaunchId,
        cancelled: &AtomicBool,
        budget: Option<&frontcap_core::write_budget::WriteBudget>,
    ) -> io::Result<T> {
        loop {
            tokio::select! {
                biased;
                result = &mut task => return result.map_err(io::Error::other)?,
                accepted = self.listener.accept() => {
                    let (stream, _) = match accepted {
                        Ok(connection) => connection,
                        Err(error) => {
                            // Do not detach a worker that may still acquire a target.
                            let _ = task.await;
                            return Err(error);
                        }
                    };
                    // A partial or abandoned client cannot indefinitely delay target readiness.
                    let _ = tokio::time::timeout(Duration::from_secs(1), async {
                        let mut stream = BufReader::new(stream);
                        let Some(request): Option<Request> = read_frame(&mut stream).await.map_err(io::Error::other)? else { return Ok::<(), io::Error>(()); };
                        match request {
                            Request::Release(request) => {
                                let reply = if cancelled.load(Ordering::Acquire) {
                                    Reply::Rejected { reason: "preparation cancellation admitted".into() }
                                } else if let Some(controller) = &controller {
                                    controller.exchange(&request.launch_id, request.command()).await
                                } else { Reply::Rejected { reason: "owned service not published".into() } };
                                write_frame(stream.get_mut(), &reply).await.map_err(io::Error::other)
                            }
                            Request::Cancel(request) => {
                                let reply = if &request.launch_id != launch {
                                    Cancellation::NotSent { reason: "preparation launch differs".into() }
                                } else if let Some(original) = frontcap_core::state::load::<Cancel>(&cancellation_path(&self.directory))? {
                                    if original == request { Cancellation::Requested { request } }
                                    else { Cancellation::NotSent { reason: "another cancellation already admitted".into() } }
                                } else {
                                    super::save_budgeted(budget, frontcap_core::write_budget::Class::Retirement,
                                        &cancellation_path(&self.directory), &request)?;
                                    cancelled.store(true, Ordering::Release);
                                    Cancellation::Requested { request }
                                };
                                write_frame(stream.get_mut(), &reply).await.map_err(io::Error::other)
                            }
                        }
                    }).await;
                }
            }
        }
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Read the original receipt first, even after acquisition/recorder exit. Never resend an uncertain effect.
pub async fn release(directory: &Path, request: Release) -> io::Result<Reply> {
    let snapshot = preparation::inspect(directory, &request.launch_id)?;
    let Some(service) = snapshot.service else {
        return Ok(Reply::Rejected {
            reason: "owned service not published".into(),
        });
    };
    if service.generation != request.generation {
        return Ok(Reply::Rejected {
            reason: "service generation differs".into(),
        });
    }
    if let Some(receipt) = service.releases.get(&request.release_id) {
        return Ok(if receipt.command == request.command() {
            Reply::Released {
                receipt: receipt.clone(),
            }
        } else {
            Reply::Rejected {
                reason: "release identity conflict".into(),
            }
        });
    }
    if snapshot.recorder_alive != Some(true)
        || !snapshot
            .runtime
            .is_some_and(|runtime| runtime.state == RuntimeState::Starting)
    {
        return Ok(Reply::Rejected {
            reason: "preparation admission closed; use the ready recorder for new releases".into(),
        });
    }
    let exchange = async {
        let stream = match UnixStream::connect(socket(directory)).await {
            Ok(stream) => stream,
            Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => {
                // Acquisition can close this endpoint before Ready is published. No request
                // bytes were sent by this attempt; do not label it an uncertain dispatch.
                return Ok(Reply::Rejected {
                    reason: "preparation connection unavailable; this attempt was not sent; inspect the original release ID and wait for the ready recorder".into(),
                });
            }
            Err(error) => return Err(error),
        };
        let mut stream = BufReader::new(stream);
        write_frame(stream.get_mut(), &request)
            .await
            .map_err(io::Error::other)?;
        read_frame(&mut stream)
            .await
            .map_err(io::Error::other)?
            .ok_or_else(|| {
                io::Error::other("preparation reply unavailable; inspect the original release ID")
            })
    };
    tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .map_err(|_| {
            io::Error::other("preparation outcome unknown; inspect the original release ID")
        })?
}


/// Ask only the original live acquisition endpoint; never adopt a saved PID or target.
/// Repeating an ID reads its original admission, including after the endpoint retires.
pub async fn cancel(directory: &Path, request: Cancel) -> io::Result<Cancellation> {
    let snapshot = preparation::inspect(directory, &request.launch_id)?;
    if let Some(original) = frontcap_core::state::load::<Cancel>(&cancellation_path(directory))? {
        return Ok(if original == request { Cancellation::Requested { request: original } }
            else { Cancellation::NotSent { reason: "another cancellation already admitted".into() } });
    }
    if snapshot.recorder_alive != Some(true) || snapshot.runtime.is_none_or(|r| r.state != RuntimeState::Starting) {
        return Ok(Cancellation::NotSent { reason: "acquisition is not starting; inspect the original runtime".into() });
    }
    let exchange = async {
        let stream = match UnixStream::connect(socket(directory)).await {
            Ok(stream) => stream,
            Err(error) if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) =>
                return Ok(Cancellation::NotSent { reason: "acquisition endpoint unavailable; request not sent".into() }),
            Err(error) => return Err(error),
        };
        let mut stream = BufReader::new(stream);
        write_frame(stream.get_mut(), &request).await.map_err(io::Error::other)?;
        read_frame(&mut stream).await.map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("cancellation reply unavailable; inspect original request"))
    };
    tokio::time::timeout(Duration::from_secs(5), exchange).await
        .map_err(|_| io::Error::other("cancellation outcome unknown; inspect original request"))?
}

#[cfg(test)]
#[path = "preparation_control_tests.rs"]
mod tests;
