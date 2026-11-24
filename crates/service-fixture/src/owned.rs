//! In-process service ownership. Dropping the owner aborts serving without inventing completion.
use super::{Control, Fixture, Record, Reply, RequestEndpoint, Shared};
use frontcap_core::{ids::LaunchId, write_budget::WriteBudget};
use serde::{Deserialize, Serialize};
use std::{io, path::PathBuf, sync::Arc};
use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub owner: LaunchId,
    pub service: LaunchId,
}

/// The recorder retains this value for exactly one mobile launch. No saved owner is adopted.
pub struct OwnedFixture {
    binding: Binding,
    initial: Record,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<Notify>,
    task: JoinHandle<io::Result<()>>,
    exit: Option<Result<(), String>>,
}

/// A control reference never owns the service task. Only the launch owner keeps serving alive.
#[derive(Clone)]
pub struct Controller {
    binding: Binding,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<Notify>,
    task: tokio::task::AbortHandle,
}

impl Controller {
    pub fn binding(&self) -> &Binding {
        &self.binding
    }

    pub async fn exchange(&self, owner: &LaunchId, command: Control) -> Reply {
        if owner != &self.binding.owner {
            return Reply::Rejected {
                reason: "service owner generation differs".into(),
            };
        }
        if self.task.is_finished() {
            return Reply::Uncertain {
                reason: "owned service task has ended".into(),
            };
        }
        let stopping = matches!(command, Control::Stop { .. });
        let (result, poisoned) = {
            let mut shared = self.shared.lock().await;
            let result = shared.control(command);
            (result, shared.poisoned)
        };
        if (stopping && result.is_ok()) || poisoned {
            self.stop.notify_one();
        }
        match result {
            Ok(reply) => reply,
            Err(error) if poisoned => Reply::Uncertain {
                reason: error.to_string(),
            },
            Err(error) => Reply::Rejected {
                reason: error.to_string(),
            },
        }
    }
}

impl OwnedFixture {
    pub async fn start(directory: PathBuf, owner: LaunchId) -> io::Result<Self> {
        Self::start_with_assets(directory, owner, Vec::new()).await
    }

    /// Freeze resource inputs before publishing ownership or accepting HTTP connections.
    pub async fn start_with_assets(
        directory: PathBuf,
        owner: LaunchId,
        assets: Vec<crate::assets::AssetInput>,
    ) -> io::Result<Self> {
        let fixture = Fixture::create_with_assets(directory.clone(), assets).await?;
        Self::serve(directory, owner, fixture)
    }

    pub async fn start_at(
        directory: PathBuf,
        owner: LaunchId,
        endpoint: RequestEndpoint,
    ) -> io::Result<Self> {
        let fixture = Fixture::create_at(directory.clone(), endpoint).await?;
        Self::serve(directory, owner, fixture)
    }

    /// Preserve an ordinary app's fixed origin while retaining explicit asset admission.
    pub async fn start_with_assets_at(
        directory: PathBuf,
        owner: LaunchId,
        assets: Vec<crate::assets::AssetInput>,
        endpoint: RequestEndpoint,
    ) -> io::Result<Self> {
        let fixture = Fixture::create_budgeted(directory.clone(), assets, Some(endpoint), None).await?;
        Self::serve(directory, owner, fixture)
    }

    pub async fn start_budgeted(
        directory: PathBuf,
        owner: LaunchId,
        assets: Vec<crate::assets::AssetInput>,
        endpoint: Option<RequestEndpoint>,
        budget: WriteBudget,
    ) -> io::Result<Self> {
        let fixture =
            Fixture::create_budgeted(directory.clone(), assets, endpoint, Some(budget)).await?;
        Self::serve(directory, owner, fixture)
    }

    fn serve(directory: PathBuf, owner: LaunchId, fixture: Fixture) -> io::Result<Self> {
        let binding = Binding {
            owner,
            service: fixture.record.generation.clone(),
        };
        // Publish ownership before serving. Failure leaves evidence, never an unowned listener task.
        // No task is serving yet; the shared state cannot be locked by another owner.
        let shared = fixture.shared.try_lock().map_err(io::Error::other)?;
        super::save_budgeted(
            shared.budget.as_ref(),
            &directory.join("owner.json"),
            &binding,
        )?;
        drop(shared);
        Ok(Self {
            binding,
            initial: fixture.record.clone(),
            shared: fixture.shared.clone(),
            stop: fixture.stop.clone(),
            // The recorder owns signal handling; an embedded fixture must not consume it independently.
            task: tokio::spawn(fixture.serve(false)),
            exit: None,
        })
    }

    pub fn binding(&self) -> &Binding {
        &self.binding
    }
    pub fn controller(&self) -> Controller {
        Controller {
            binding: self.binding.clone(),
            shared: self.shared.clone(),
            stop: self.stop.clone(),
            task: self.task.abort_handle(),
        }
    }
    /// Startup endpoint only. Use inspect for current request/release state.
    pub fn initial(&self) -> &Record {
        &self.initial
    }

    pub async fn control(&self, owner: &LaunchId, command: Control) -> io::Result<Reply> {
        match self.controller().exchange(owner, command).await {
            Reply::Rejected { reason } | Reply::Uncertain { reason } => {
                Err(io::Error::other(reason))
            }
            reply => Ok(reply),
        }
    }

    /// Completes only after the listener and its owned connections have retired.
    pub async fn shutdown(mut self) -> io::Result<()> {
        self.stop.notify_one();
        self.wait_for_exit().await
    }

    /// Cancellation-safe observation: abandoning this wait neither stops nor detaches the service.
    /// Repeated waits retain the first terminal result, including task failures.
    pub async fn wait_for_exit(&mut self) -> io::Result<()> {
        if self.exit.is_none() {
            let result = match (&mut self.task).await {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            self.exit = Some(result);
        }
        self.exit
            .as_ref()
            .unwrap()
            .clone()
            .map_err(io::Error::other)
    }
}

impl Drop for OwnedFixture {
    fn drop(&mut self) {
        // JoinHandle normally detaches on drop. Never leave a service running after its owner goes.
        self.task.abort();
    }
}
