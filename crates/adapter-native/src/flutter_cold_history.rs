//! Cold entry history shares ordinary replacement epochs but keeps begin and poll identities.
use super::*;
use crate::flutter_ios_entry::{self, Polled, Started};
use frontcap_core::{
    canonical::sha256_bytes_hex,
    native::flutter_entry::{Pending, Progress},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cold {
    admission_sha256: String,
    started: Option<Started>,
    polls: Vec<Poll>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Poll {
    id: RequestId,
    command_number: u64,
    completion: Option<Polled>,
}
impl Cold {
    pub(super) fn contains(&self, id: &RequestId) -> bool {
        self.polls.iter().any(|p| &p.id == id)
    }
    pub(super) fn validate<'a>(
        &'a self,
        entry: &'a Entry,
        ready: &Ready,
        app: &Acquired,
        ids: &mut HashSet<&'a RequestId>,
        number: &mut u64,
    ) -> io::Result<()> {
        let request = flutter_ios_entry::request(
            ready,
            entry
                .url
                .as_deref()
                .ok_or_else(|| io::Error::other("cold entry URL missing"))?,
        )?;
        if self.admission_sha256.len() != 64
            || !self
                .admission_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(io::Error::other("invalid cold entry admission digest"));
        }
        let Some(started) = &self.started else {
            return if self.polls.is_empty() && entry.completion.is_none() {
                Ok(())
            } else {
                Err(io::Error::other("cold readiness precedes acceptance"))
            };
        };
        started.validate(app, ready, &request)?;
        if started.command_number != entry.command_number {
            return Err(io::Error::other("cold acceptance command differs"));
        }
        let mut previous = None;
        let mut replacement = None;
        for (index, poll) in self.polls.iter().enumerate() {
            if !ids.insert(&poll.id) || poll.command_number <= *number {
                return Err(io::Error::other(
                    "cold poll identity or command sequence differs",
                ));
            }
            *number = poll.command_number;
            if let Some(result) = &poll.completion {
                result.validate(started, ready, previous)?;
                if let Polled::Ready {
                    replacement: result,
                    ..
                } = result
                {
                    if index + 1 != self.polls.len() {
                        return Err(io::Error::other("poll follows ready replacement"));
                    }
                    replacement = Some(result);
                }
                previous = Some(result);
            } else if index + 1 != self.polls.len() {
                return Err(io::Error::other("poll follows unresolved readiness"));
            }
        }
        if entry.completion.as_ref() != replacement.map(|v| &**v) {
            return Err(io::Error::other("cold epoch differs from last ready poll"));
        }
        Ok(())
    }
    pub(super) fn verify_worker(
        &self,
        root: &Path,
        entry: &Entry,
        ready: &Ready,
        app: &Acquired,
    ) -> io::Result<()> {
        let started = read_started(root, entry, ready, app)?;
        if self.started.is_some() && self.started != started {
            return Err(io::Error::other("saved cold acceptance changed"));
        }
        for poll in &self.polls {
            let actual = read_worker_poll(root, entry, poll)?;
            if poll.completion.is_some() && poll.completion != actual {
                return Err(io::Error::other("saved cold readiness changed"));
            }
        }
        Ok(())
    }
}
fn pending(entry: &Entry, ready: &Ready, started: &Started) -> io::Result<Pending> {
    let value = Pending {
        request_id: entry.id.clone(),
        previous: ready.owner.clone(),
        acceptance: started.entry.clone(),
    };
    value.validate().map_err(io::Error::other)?;
    Ok(value)
}
fn progress(
    entry: &Entry,
    ready: &Ready,
    started: &Started,
    result: &Polled,
    previous: Option<&Polled>,
) -> io::Result<Progress> {
    let replacement = result.validate(started, ready, previous)?;
    let entry = pending(entry, ready, started)?;
    let value = match result {
        Polled::Waiting { readiness, .. } => Progress::Waiting {
            entry,
            readiness: readiness.clone(),
        },
        Polled::Ready { .. } => Progress::Ready {
            entry,
            replacement: Box::new(replacement.unwrap()),
        },
    };
    value.validate().map_err(io::Error::other)?;
    Ok(value)
}
impl History {
    pub fn dispatch_entry(
        &mut self,
        id: RequestId,
        generation: String,
        url: String,
        command_number: u64,
        admission_sha256: String,
    ) -> io::Result<()> {
        self.ensure_resolved()?;
        if self.contains(&id) || self.state.current().0.owner.generation != generation {
            return Err(io::Error::other("cold entry ID or generation differs"));
        }
        let mut state = self.state.clone();
        state.entries.push(Entry {
            id,
            generation,
            url: Some(url),
            command_number,
            completion: None,
            cold: Some(Cold {
                admission_sha256,
                started: None,
                polls: vec![],
            }),
        });
        self.publish(state)
    }
    pub fn accept_entry(&mut self, id: &RequestId, started: Started) -> io::Result<Pending> {
        let mut state = self.state.clone();
        let (ready, app) = self.state.current();
        let entry = state
            .entries
            .last_mut()
            .ok_or_else(|| io::Error::other("cold intent missing"))?;
        if &entry.id != id || entry.cold.as_ref().is_none_or(|c| c.started.is_some()) {
            return Err(io::Error::other(
                "cold acceptance has no original pending intent",
            ));
        }
        if read_started(&self.root, entry, ready, app)?.as_ref() != Some(&started) {
            return Err(io::Error::other(
                "cold acceptance differs from original worker result",
            ));
        }
        let result = pending(entry, ready, &started)?;
        entry.cold.as_mut().unwrap().started = Some(started);
        self.publish(state)?;
        Ok(result)
    }
    /// System work is allowed only after accepted begin and between fully recorded waiting polls.
    pub fn ensure_system_available(&self) -> io::Result<()> {
        if self.ensure_resolved().is_ok() {
            return Ok(());
        }
        if !self.poisoned
            && let Some(entry) = self.state.entries.last()
            && entry.completion.is_none()
            && let Some(cold) = &entry.cold
            && cold.started.is_some()
            && cold
                .polls
                .iter()
                .all(|p| matches!(p.completion, Some(Polled::Waiting { .. })))
        {
            return Ok(());
        }
        Err(io::Error::other(
            "Flutter lifecycle unresolved; system input is fenced",
        ))
    }
    pub fn pending_entry(&self) -> io::Result<Option<Pending>> {
        if self.poisoned {
            return Err(io::Error::other("cold history publication uncertain"));
        }
        let (ready, _) = self.state.current();
        match self.state.entries.last() {
            Some(entry) if entry.completion.is_none() => {
                match entry.cold.as_ref().and_then(|c| c.started.as_ref()) {
                    Some(started) => pending(entry, ready, started).map(Some),
                    None => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }
    pub fn validate_poll(
        &self,
        id: &RequestId,
        begin: &RequestId,
        generation: &str,
    ) -> io::Result<()> {
        self.ensure_system_available()?;
        if self.contains(id) {
            return Err(io::Error::other(
                "cold poll request ID already belongs to lifecycle history",
            ));
        }
        let value = self
            .pending_entry()?
            .ok_or_else(|| io::Error::other("no live pending cold entry"))?;
        if &value.request_id != begin || value.previous.generation != generation {
            return Err(io::Error::other(
                "cold poll differs from original begin request",
            ));
        }
        Ok(())
    }
    pub fn dispatch_poll(
        &mut self,
        id: RequestId,
        begin: &RequestId,
        generation: &str,
        command_number: u64,
    ) -> io::Result<(Started, Option<Polled>)> {
        self.validate_poll(&id, begin, generation)?;
        let mut state = self.state.clone();
        let cold = state.entries.last_mut().unwrap().cold.as_mut().unwrap();
        let previous = cold.polls.last().and_then(|p| p.completion.clone());
        let started = cold.started.clone().unwrap();
        cold.polls.push(Poll {
            id,
            command_number,
            completion: None,
        });
        self.publish(state)?;
        Ok((started, previous))
    }
    pub fn validate_polled(&self, id: &RequestId, result: &Polled) -> io::Result<Progress> {
        let entry = self
            .state
            .entries
            .last()
            .ok_or_else(|| io::Error::other("cold intent missing"))?;
        let cold = entry
            .cold
            .as_ref()
            .ok_or_else(|| io::Error::other("not a cold entry"))?;
        let poll = cold
            .polls
            .last()
            .ok_or_else(|| io::Error::other("cold poll intent missing"))?;
        if &poll.id != id || poll.completion.is_some() {
            return Err(io::Error::other("cold poll cannot complete twice"));
        }
        let started = cold
            .started
            .as_ref()
            .ok_or_else(|| io::Error::other("cold acceptance unresolved"))?;
        let previous = cold
            .polls
            .iter()
            .rev()
            .nth(1)
            .and_then(|p| p.completion.as_ref());
        let value = progress(entry, self.state.current().0, started, result, previous)?;
        if let Polled::Ready { replacement, .. } = result {
            self.validate_completion(replacement)?;
        }
        if read_worker_poll(&self.root, entry, poll)?.as_ref() != Some(result) {
            return Err(io::Error::other(
                "cold readiness differs from original worker result",
            ));
        }
        Ok(value)
    }
    pub fn complete_poll(&mut self, id: &RequestId, result: Polled) -> io::Result<Progress> {
        let value = self.validate_polled(id, &result)?;
        let mut state = self.state.clone();
        let entry = state.entries.last_mut().unwrap();
        if let Polled::Ready { replacement, .. } = &result {
            entry.completion = Some(replacement.as_ref().clone());
        }
        entry
            .cold
            .as_mut()
            .unwrap()
            .polls
            .last_mut()
            .unwrap()
            .completion = Some(result);
        self.publish(state)?;
        Ok(value)
    }
}
#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    BeginEntry {
        number: u64,
        timeout_ms: u64,
        generation: String,
        url: String,
    },
    PollEntry {
        number: u64,
        timeout_ms: u64,
        generation: String,
        begin_command: u64,
    },
}
fn worker_directory(root: &Path, number: u64) -> PathBuf {
    root.join("session/worker/commands")
        .join(number.to_string())
}
fn read_started(
    root: &Path,
    entry: &Entry,
    ready: &Ready,
    app: &Acquired,
) -> io::Result<Option<Started>> {
    let cold = entry
        .cold
        .as_ref()
        .ok_or_else(|| io::Error::other("not a cold entry"))?;
    let admission = root
        .join("session/worker/entry-admissions")
        .join(format!("{}.json", entry.command_number));
    if sha256_bytes_hex(&std::fs::read(admission)?) != cold.admission_sha256 {
        return Err(io::Error::other("cold admission evidence changed"));
    }
    let directory = worker_directory(root, entry.command_number);
    let Some(command): Option<Command> = load(&directory.join("request.json"))? else {
        return Ok(None);
    };
    if !matches!(command, Command::BeginEntry { number, timeout_ms, generation, url }
        if number == entry.command_number && (1..=120_000).contains(&timeout_ms)
            && generation == entry.generation && Some(&url) == entry.url.as_ref())
    {
        return Err(io::Error::other("cold begin worker binding differs"));
    }
    let result: Option<Started> = load(&directory.join("command.log"))?;
    if let Some(result) = &result {
        let request = flutter_ios_entry::request(ready, entry.url.as_deref().unwrap())?;
        result.validate(app, ready, &request)?;
        if result.command_number != entry.command_number {
            return Err(io::Error::other("cold begin command changed"));
        }
    }
    Ok(result)
}
fn read_worker_poll(root: &Path, entry: &Entry, poll: &Poll) -> io::Result<Option<Polled>> {
    let directory = worker_directory(root, poll.command_number);
    let Some(command): Option<Command> = load(&directory.join("request.json"))? else {
        return Ok(None);
    };
    if !matches!(command, Command::PollEntry { number, timeout_ms, generation, begin_command }
        if number == poll.command_number && (1..=120_000).contains(&timeout_ms)
            && generation == entry.generation && begin_command == entry.command_number)
    {
        return Err(io::Error::other("cold poll worker binding differs"));
    }
    load(&directory.join("command.log"))
}
pub(super) fn read_replacement(root: &Path, entry: &Entry) -> io::Result<Option<Restarted>> {
    match entry.cold.as_ref().and_then(|c| c.polls.last()) {
        Some(poll) => Ok(match read_worker_poll(root, entry, poll)? {
            Some(Polled::Ready { replacement, .. }) => Some(*replacement),
            _ => None,
        }),
        None => Ok(None),
    }
}
pub fn read_entry(
    root: &Path,
    id: &RequestId,
    generation: &str,
    url: &str,
) -> io::Result<Option<Pending>> {
    let Some(state) = read(root)? else {
        return Ok(None);
    };
    let mut current = (&state.initial, &state.application);
    for entry in &state.entries {
        if &entry.id == id {
            if entry.cold.is_none()
                || entry.generation != generation
                || entry.url.as_deref() != Some(url)
            {
                return Err(io::Error::other("cold begin request conflicts"));
            }
            return read_started(root, entry, current.0, current.1)?
                .map(|s| pending(entry, current.0, &s))
                .transpose();
        }
        if let Some(done) = &entry.completion {
            current = (&done.ready, &done.current);
        }
    }
    Ok(None)
}
pub fn read_poll(
    root: &Path,
    id: &RequestId,
    begin: &RequestId,
    generation: &str,
) -> io::Result<Option<Progress>> {
    let Some(state) = read(root)? else {
        return Ok(None);
    };
    let mut current = (&state.initial, &state.application);
    let mut generations = HashSet::from([&state.initial.owner.generation]);
    for entry in &state.entries {
        if let Some(cold) = &entry.cold {
            for (index, poll) in cold.polls.iter().enumerate() {
                if &poll.id != id {
                    continue;
                }
                if &entry.id != begin || entry.generation != generation {
                    return Err(io::Error::other("cold poll request conflicts"));
                }
                let started = read_started(root, entry, current.0, current.1)?
                    .ok_or_else(|| io::Error::other("cold poll has no original acceptance"))?;
                let previous = index
                    .checked_sub(1)
                    .and_then(|i| cold.polls[i].completion.as_ref());
                return read_worker_poll(root, entry, poll)?
                    .map(|result| {
                        let value = progress(entry, current.0, &started, &result, previous)?;
                        if let Progress::Ready { replacement, .. } = &value
                            && generations.contains(&replacement.owner().generation)
                        {
                            return Err(io::Error::other("cold readiness reused a generation"));
                        }
                        Ok(value)
                    })
                    .transpose();
            }
        }
        if let Some(done) = &entry.completion {
            current = (&done.ready, &done.current);
            generations.insert(&done.ready.owner.generation);
        }
    }
    Ok(None)
}
