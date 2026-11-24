//! Client handles for a Simulator whose creation and cleanup remain in a live resource group.
use super::*;
use crate::{resource_group::{Group, Operation, Reply}, resource_handle::{Shared, Process, with_group}};
use frontcap_core::{ids::RequestId, state::load};
use std::sync::{Arc, Mutex};

impl Simulator {
    /// Consume a live group; never reconstruct authority from its on-disk records.
    pub fn acquire_supervised(config: SimulatorLaunch, mut group: Group, deadline: Instant,
        cancelled: &AtomicBool) -> Result<Self, AcquisitionError> {
        let directory = group.ready.target.join("simulator");
        let result = (|| -> io::Result<(String, u64, String)> {
            let deadline_ns = crate::worker::monotonic_ns()?.saturating_add(
                u64::try_from(deadline.saturating_duration_since(Instant::now()).as_nanos()).map_err(io::Error::other)?);
            let reply = group.execute_until(Operation::AcquireSimulator { config: config.clone(), deadline_ns }, deadline, cancelled)?;
            let Reply::SimulatorAcquired { udid, command_number } = reply else {
                return Err(io::Error::other(format!("supervised Simulator acquisition unresolved: {reply:?}")));
            };
            let name = load(&directory.join("name.json"))?.ok_or_else(|| io::Error::other("created Simulator name absent"))?;
            Ok((udid, command_number, name))
        })();
        match result {
            Ok((udid, number, name)) => Ok(Self { config, directory, host_record: None, lease: None,
                name, udid: Some(udid), create_started: true, number, unretired_commands: vec![],
                retired: None, supervision: Some(Arc::new(Mutex::new(Some(group)))), resource_number: 0, data_volume: None, data_volume_attempted: false }),
            Err(error) => {
                let retirement = group.stop(Instant::now() + Duration::from_secs(75))
                    .map(group_retirement).unwrap_or_else(|error| Retirement {
                        passed: false, errors: vec![error.to_string()], signal_error: None });
                Err(AcquisitionError { cause: error.to_string(), retirement: Some(retirement) })
            }
        }
    }
    pub(super) fn supervised_command(&mut self, args: &[&str], environment: &BTreeMap<String, String>,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<Vec<u8>> {
        let deadline_ns = crate::worker::monotonic_ns()?.saturating_add(
            u64::try_from(deadline.saturating_duration_since(Instant::now()).as_nanos()).map_err(io::Error::other)?);
        let reply = with_group(self.supervision.as_ref().unwrap(), |group| group.execute_until(
            Operation::SimulatorCommand { arguments: args.iter().map(|s| (*s).into()).collect(),
                environment: environment.clone(), deadline_ns }, deadline, cancelled))?;
        let Reply::SimulatorCommandCompleted { command_number, evidence } = reply else {
            return Err(io::Error::other(format!("supervised Simulator command unresolved: {reply:?}")));
        };
        if command_number != self.number + 1 || evidence != self.directory.join(format!("command-{command_number}/exchange.json")) {
            return Err(io::Error::other("Simulator command cursor or original evidence differs"));
        }
        self.number = command_number;
        let exchange: crate::process::Exchange = crate::process::load_exchange(&evidence)?.ok_or_else(|| io::Error::other("Simulator exchange missing"))?;
        if let Some(error) = exchange.failure.or(exchange.cleanup_failure).or(exchange.evidence_failure) {
            return Err(io::Error::other(error));
        }
        Ok(exchange.stdout)
    }
    pub(crate) fn capture_owned(&mut self, command: &mut Command, deadline: Instant,
        cancelled: &AtomicBool, limit: usize, evidence: &Path) -> io::Result<crate::process::Exchange> {
        if let Some(shared) = &self.supervision {
            with_group(shared, |group| group.capture(command, evidence, deadline, cancelled, limit))
        } else { capture_recorded(command, deadline, cancelled, limit, evidence) }
    }
    pub(crate) fn supervision(&self) -> Option<Shared> { self.supervision.clone() }
    pub(crate) fn spawn_owned(&mut self, command: &mut Command, evidence: &Path,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<Process> {
        if let Some(shared) = &self.supervision {
            self.resource_number += 1;
            let id = RequestId::new(format!("runner-{}", self.resource_number)).map_err(io::Error::other)?;
            let identity = with_group(shared, |group| group.start_resource(id.clone(), command, deadline, cancelled))?;
            save_atomic(&evidence.join("resource.json"), &id)?;
            Ok(Process::Supervised { group: shared.clone(), id, identity, result: None })
        } else { Ok(Process::Direct(crate::process::OwnedChild::new(command.spawn()?))) }
    }
    pub(super) fn stop_supervised(&mut self, additional: Option<&Path>, signal_error: Option<String>) -> Retirement {
        let result = (|| {
            let group = self.supervision.as_ref().unwrap().lock().map_err(|_| io::Error::other("resource group lock poisoned"))?
                .take().ok_or_else(|| io::Error::other("resource group already retired"))?;
            group.stop(Instant::now() + Duration::from_secs(75))
        })();
        let mut result = result.map(group_retirement).unwrap_or_else(|error|
            Retirement { passed: false, errors: vec![error.to_string()], signal_error: None });
        result.errors.extend(self.unretired_commands.clone());
        if signal_error.is_some() { result.signal_error = signal_error; }
        result.passed &= result.errors.is_empty();
        // The group owns simulator/retirement.json. A client may publish only its higher-level result.
        if let Some(path) = additional && let Err(error) = save_atomic(path, &result) {
            result.passed = false; result.errors.push(error.to_string());
        }
        self.retired = Some(result.clone());
        result
    }
}
fn group_retirement(group: crate::resource_group::Retirement) -> Retirement {
    let signal_error = group.resources.values().find_map(|resource| resource.signal_error.clone());
    Retirement { passed: group.passed, errors: group.errors, signal_error }
}
