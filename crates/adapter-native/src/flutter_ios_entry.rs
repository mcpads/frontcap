//! Two-phase worker evidence. Saved acceptance and readiness cannot grant a live device lease.
use crate::{
    flutter_app::{Acquired, Restarted},
    flutter_ios_app,
    flutter_session::Ready,
};
use frontcap_core::native::{
    external_entry::{Accepted, Request},
    flutter_lifecycle::{self, Process, Replacement},
};
use serde::{Deserialize, Serialize};
use std::io;

pub fn request(ready: &Ready, url: &str) -> io::Result<Request> {
    let request =
        flutter_lifecycle::ios_entry(&ready.owner, ready.external_url_target.as_ref(), url)
            .map_err(io::Error::other)?;
    let system = ready
        .system_ui
        .as_ref()
        .ok_or_else(|| io::Error::other("cold iOS entry needs an independent system runner"))?;
    system.validate().map_err(io::Error::other)?;
    if !system.simulator.eq_ignore_ascii_case(&ready.owner.device) {
        return Err(io::Error::other(
            "cold iOS system runner belongs to another device",
        ));
    }
    Ok(request)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Started {
    pub command_number: u64,
    pub previous: flutter_ios_app::Acquired,
    pub entry: Accepted,
}
impl Started {
    pub fn validate(
        &self,
        previous: &Acquired,
        ready: &Ready,
        expected: &Request,
    ) -> io::Result<()> {
        self.previous.validate(ready)?;
        if self.command_number == 0
            || previous != &Acquired::Ios(self.previous.clone())
            || &self.entry.request != expected
            || request(ready, &expected.url)? != *expected
        {
            return Err(io::Error::other(
                "cold iOS acceptance differs from original owner or request",
            ));
        }
        Ok(())
    }
}

pub use frontcap_core::native::flutter_entry::Waiting;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Polled {
    Waiting {
        begin_command: u64,
        readiness: Waiting,
    },
    Ready {
        begin_command: u64,
        replacement: Box<Restarted>,
    },
}
impl Polled {
    fn process(&self) -> Option<&Process> {
        match self {
            Self::Waiting {
                readiness: Waiting::Endpoint { process },
                ..
            } => Some(process),
            Self::Ready { replacement, .. } => match &replacement.current {
                Acquired::Ios(app) => Some(&app.process),
                _ => None,
            },
            _ => None,
        }
    }
    pub fn validate(
        &self,
        started: &Started,
        ready: &Ready,
        previous_poll: Option<&Self>,
    ) -> io::Result<Option<Replacement>> {
        started.validate(
            &Acquired::Ios(started.previous.clone()),
            ready,
            &started.entry.request,
        )?;
        let (Self::Waiting { begin_command, .. } | Self::Ready { begin_command, .. }) = self;
        if *begin_command != started.command_number {
            return Err(io::Error::other(
                "cold iOS poll belongs to another dispatch",
            ));
        }
        if let Some(previous) = previous_poll {
            previous.validate(started, ready, None)?;
            if matches!(previous, Self::Ready { .. })
                || previous
                    .process()
                    .is_some_and(|p| self.process() != Some(p))
            {
                return Err(io::Error::other(
                    "cold iOS poll replaces an already observed process or readiness",
                ));
            }
        }
        if let Some(process) = self.process() {
            process.validate().map_err(io::Error::other)?;
            if (process.pid == started.previous.process.pid
                && process.birth == started.previous.process.birth)
                || process.app_executable(&started.previous.containers.app).map_err(io::Error::other)?
                    != started.previous.process.app_executable(&started.previous.containers.app).map_err(io::Error::other)?
            {
                return Err(io::Error::other(
                    "cold iOS poll has an old or foreign process",
                ));
            }
        }
        match self {
            Self::Waiting { .. } => Ok(None),
            Self::Ready { replacement, .. } => {
                if replacement.entry.as_ref() != Some(&started.entry) {
                    return Err(io::Error::other(
                        "cold iOS readiness changed the original URL acceptance",
                    ));
                }
                replacement
                    .validate(&Acquired::Ios(started.previous.clone()), ready)
                    .map(Some)
            }
        }
    }
}

#[cfg(test)]
#[path = "flutter_ios_entry_tests.rs"]
mod tests;
