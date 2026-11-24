//! Bounded passive ART speech evidence. Consumer tokens are not unique requests.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Kind {
    #[serde(rename = "speak")]
    Request,
    #[serde(rename = "stop")]
    StopRequest,
    #[serde(rename = "onStart")]
    Started,
    #[serde(rename = "onSuccess")]
    Finished,
    #[serde(rename = "onError")]
    Failed,
    #[serde(rename = "onStop")]
    Cancelled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Correlation {
    Unattributed,
    SingleObservedRequest,
    ReusedToken,
}
impl Correlation {
    fn for_requests(count: u32) -> Self {
        match count {
            0 => Self::Unattributed,
            1 => Self::SingleObservedRequest,
            _ => Self::ReusedToken,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub sequence: u32,
    pub kind: Kind,
    pub identity: u32,
    pub request_count: u32,
    pub correlation: Correlation,
    pub local_error: i32,
    pub observer_exception: bool,
    pub monotonic_ns: u64,
    pub limit_reached: bool,
    #[serde(default, skip_serializing_if = "zero")]
    pub synthesizer: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub identity: u32,
    pub requests: u32,
    pub starts: u32,
    pub finishes: u32,
    pub cancellations: u32,
    pub failures: u32,
    pub correlation: Correlation,
    pub last_sequence: u32,
    pub last_kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesizer: Option<u32>,
}
/// Counts survive tail eviction. A last start is not an independent current-state sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<Attachment>,
    #[serde(default, skip_serializing_if = "not")]
    pub sampling: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<Sample>,
    #[serde(default)]
    pub submission_observed: bool,
    #[serde(default)]
    pub submissions: Vec<Submission>,
    #[serde(default)]
    pub pending_submissions: Vec<u32>,
    pub bound_kinds: Vec<Kind>,
    pub received: u32,
    pub dropped: u32,
    pub truncated: bool,
    pub faults: u32,
    pub unattributed: u32,
    pub events: Vec<Event>,
    pub tokens: Vec<Token>,
}

/// Bound to the exact observed call, even when consumer tokens are reused.
/// Zero means accepted for submission, not started or finished. Null with neither
/// an exception nor an observer error is invalid; a missing return remains pending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    pub request_sequence: u32,
    pub identity: u32,
    pub returned: Option<i32>,
    pub threw: bool,
    pub observer_error: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub id: String,
    pub start_ticks: u64,
}
fn zero(value: &u32) -> bool {
    *value == 0
}
fn not(value: &bool) -> bool {
    !value
}

/// Framework state sampled during an explicit host request, separate from callback history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    pub request: String,
    pub attachment: String,
    pub pid: u32,
    pub start_ticks: u64,
    pub request_index: u64,
    pub sequence_before: u32,
    pub sequence_after: u32,
    pub began_ns: u64,
    pub ended_ns: u64,
    pub faults: u32,
    pub reference_limit_reached: bool,
    pub synthesizers: Vec<Synthesizer>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Synthesizer {
    pub id: u32,
    pub state: SampleState,
    pub speaking: Option<bool>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleState {
    Sampled,
    Collected,
    Failed,
}

impl History {
    pub fn apply_sample(&mut self, raw: &str, request: &str, request_index: u64) -> io::Result<()> {
        if raw.len() > 8192 {
            return Err(io::Error::other("speech state sample exceeds bound"));
        }
        let sample: Sample = serde_json::from_str(raw).map_err(io::Error::other)?;
        let valid_identity = self.attachment.as_ref().is_some_and(|identity| {
            sample.attachment == identity.id && sample.start_ticks == identity.start_ticks
        });
        if !self.sampling
            || !valid_identity
            || sample.pid != self.pid
            || sample.request != request
            || request.len() != 32
            || !request
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            || request_index == 0
            || sample.request_index != request_index
            || sample.began_ns == 0
            || sample.ended_ns < sample.began_ns
            || sample.sequence_before > sample.sequence_after
            || sample.sequence_after > self.received
            || sample.faults > 4096
            || sample.synthesizers.len() > 8
            || sample
                .synthesizers
                .iter()
                .enumerate()
                .any(|(index, value)| {
                    value.id != index as u32 + 1
                        || (value.state == SampleState::Sampled) != value.speaking.is_some()
                })
        {
            return Err(io::Error::other(
                "speech state sample differs from request, owner or observation bounds",
            ));
        }
        self.sample = Some(sample);
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    kind: Kind,
    local_table_error: i32,
    identity_slot: i32,
    breakpoint_error: i32,
    rate_slot: i32,
    format_slot: i32,
    channels_slot: i32,
    audio_slot: i32,
    text_slot: i32,
    params_slot: i32,
}
#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
enum Row {
    Attach {
        pid: u32,
        get_env: i32,
        #[serde(default)]
        attachment: Option<String>,
        #[serde(default)]
        start_ticks: Option<u64>,
    },
    Bind(Binding),
    Ready {
        pid: u32,
        boundaries: u32,
        flow_only: bool,
        #[serde(default)]
        sampling: bool,
        #[serde(default)]
        submission_observed: bool,
    },
    Boundary(Event),
    Submission(Submission),
}

impl History {
    pub fn from_bound_jsonl(raw: &str, pid: u32, attachment: &Attachment) -> io::Result<Self> {
        let history = Self::from_jsonl(raw, pid)?;
        if history.attachment.as_ref() != Some(attachment) {
            return Err(io::Error::other(
                "Android speech attachment or process birth differs",
            ));
        }
        Ok(history)
    }
    /// Decode only complete snapshots of the lifecycle-only producer. The caller must
    /// additionally verify process birth and attachment identity before and after reading.
    pub fn from_jsonl(raw: &str, expected_pid: u32) -> io::Result<Self> {
        let invalid = || io::Error::other("invalid or incomplete Android speech flow evidence");
        if expected_pid == 0
            || expected_pid > i32::MAX as u32
            || raw.len() > 262_144
            || !raw.ends_with('\n')
        {
            return Err(invalid());
        }
        let mut result = Self {
            pid: expected_pid,
            attachment: None,
            sampling: false,
            sample: None,
            submission_observed: false,
            submissions: vec![],
            pending_submissions: vec![],
            bound_kinds: vec![],
            received: 0,
            dropped: 0,
            truncated: false,
            faults: 0,
            unattributed: 0,
            events: vec![],
            tokens: vec![],
        };
        let mut attached = false;
        let mut ready = false;
        let mut bindings = 0;
        let mut successful_bindings = 0;
        let mut previous_ns = 0;
        let mut tokens: BTreeMap<u32, Token> = BTreeMap::new();
        let mut pending_submissions = BTreeMap::new();
        for (index, line) in raw.lines().enumerate() {
            // 128 bindings, attach/ready, 256 lifecycle entries and at most one
            // submission result per entry. The existing byte bound is unchanged.
            if index >= 642 || line.len() > 4096 {
                return Err(invalid());
            }
            let row: Row = serde_json::from_str(line).map_err(io::Error::other)?;
            match row {
                Row::Attach {
                    pid,
                    get_env,
                    attachment,
                    start_ticks,
                } => {
                    if index != 0 || pid != expected_pid || get_env != 0 {
                        return Err(invalid());
                    }
                    attached = true;
                    result.attachment = match (attachment, start_ticks) {
                        (None, None) => None,
                        (Some(id), Some(start_ticks))
                            if super::request::canonical_uuid(&id) && start_ticks > 0 =>
                        {
                            Some(Attachment { id, start_ticks })
                        }
                        _ => return Err(invalid()),
                    };
                }
                _ if !attached => return Err(invalid()),
                Row::Bind(binding) => {
                    bindings += 1;
                    if bindings > 128
                        || [
                            binding.identity_slot,
                            binding.rate_slot,
                            binding.format_slot,
                            binding.channels_slot,
                            binding.audio_slot,
                            binding.text_slot,
                            binding.params_slot,
                        ]
                        .iter()
                        .any(|n| *n < -1)
                    {
                        return Err(invalid());
                    }
                    if binding.breakpoint_error == 0 {
                        successful_bindings += 1;
                        if !result.bound_kinds.contains(&binding.kind) {
                            result.bound_kinds.push(binding.kind);
                        }
                    }
                    if binding.breakpoint_error != 0
                        || binding.local_table_error != 0
                        || (binding.kind != Kind::StopRequest && binding.identity_slot < 0)
                    {
                        result.faults += 1;
                    }
                }
                Row::Ready {
                    pid,
                    boundaries,
                    flow_only,
                    sampling,
                    submission_observed,
                } => {
                    if ready
                        || pid != expected_pid
                        || !flow_only
                        || boundaries == 0
                        || boundaries != successful_bindings
                    {
                        return Err(invalid());
                    }
                    ready = true;
                    if sampling && result.attachment.is_none() {
                        return Err(invalid());
                    }
                    result.sampling = sampling;
                    result.submission_observed = submission_observed;
                }
                Row::Submission(submission) => {
                    if pending_submissions.remove(&submission.request_sequence) != Some(submission.identity)
                        || submission.observer_error < 0
                        || (submission.returned.is_some()
                            != (!submission.threw && submission.observer_error == 0))
                        || (submission.threw && submission.observer_error != 0)
                    {
                        return Err(invalid());
                    }
                    if submission.observer_error != 0 { result.faults += 1; }
                    result.submissions.push(submission);
                }
                Row::Boundary(event) => {
                    if event.sequence != result.received + 1
                        || event.sequence > 256
                        || event.identity > 64
                        || event.synthesizer > 8
                        || (!matches!(event.kind, Kind::Request | Kind::StopRequest) && event.synthesizer != 0)
                        || event.monotonic_ns < previous_ns
                        || event.limit_reached != (event.sequence == 256)
                        || !result.bound_kinds.contains(&event.kind)
                        || (event.kind == Kind::StopRequest && event.identity != 0)
                    {
                        return Err(invalid());
                    }
                    previous_ns = event.monotonic_ns;
                    if event.kind == Kind::Request {
                        pending_submissions.insert(event.sequence, event.identity);
                    }
                    let requests = if event.identity == 0 {
                        result.unattributed += 1;
                        0
                    } else {
                        let token = tokens.entry(event.identity).or_insert(Token {
                            identity: event.identity,
                            requests: 0,
                            starts: 0,
                            finishes: 0,
                            cancellations: 0,
                            failures: 0,
                            correlation: Correlation::Unattributed,
                            last_sequence: 0,
                            last_kind: event.kind,
                            synthesizer: None,
                        });
                        match event.kind {
                            Kind::Request => {
                                token.requests += 1;
                                token.synthesizer = if token.requests == 1 && event.synthesizer > 0
                                {
                                    Some(event.synthesizer)
                                } else {
                                    None
                                };
                            }
                            Kind::Started => token.starts += 1,
                            Kind::Finished => token.finishes += 1,
                            Kind::Cancelled => token.cancellations += 1,
                            Kind::Failed => token.failures += 1,
                            Kind::StopRequest => return Err(invalid()),
                        }
                        token.last_sequence = event.sequence;
                        token.last_kind = event.kind;
                        token.correlation = Correlation::for_requests(token.requests);
                        token.requests
                    };
                    if event.request_count != requests
                        || event.correlation != Correlation::for_requests(requests)
                    {
                        return Err(invalid());
                    }
                    if event.observer_exception
                        || (event.local_error != 0
                            && !(event.kind == Kind::StopRequest && event.local_error == 101))
                    {
                        result.faults += 1;
                    }
                    result.received += 1;
                    result.truncated = event.limit_reached;
                    result.events.push(event);
                }
            }
        }
        if !ready {
            return Err(invalid());
        }
        if result.submission_observed {
            result.pending_submissions = pending_submissions.into_keys().collect();
        } else if !result.submissions.is_empty() {
            return Err(invalid());
        }
        result.dropped = result.received.saturating_sub(32);
        result.events.drain(..result.dropped as usize);
        result.bound_kinds.sort();
        result.tokens = tokens.into_values().collect();
        Ok(result)
    }
}

#[cfg(test)]
#[path = "android_speech_tests.rs"]
mod tests;
