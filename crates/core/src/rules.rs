use crate::canonical::{canonical_of, sha256_hex};
use crate::ids::RuleId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

/// The whole request URL, exactly, except that `*` (the only wildcard) matches any run of characters:
/// `https://api.example.test/*` matches every path of that origin.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UrlPattern(pub String);

impl UrlPattern {
    pub fn matches(&self, url: &str) -> bool {
        glob(self.0.as_bytes(), url.as_bytes())
    }
}

/// Iterative glob match with `*` as the only wildcard.
pub(crate) fn glob(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Matcher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    pub url: UrlPattern,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// JSON Pointer (`/code`, `/items/0/id`) → the value the request's JSON body has there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<BTreeMap<String, serde_json::Value>>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockSpec {
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: String,
    /// Raw bytes, base64, instead of `body`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_base64: Option<String>,
    #[serde(default)]
    pub latency_ms: u64,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfterLast {
    RepeatLast,
    Fail,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailReason {
    ConnectionRefused,
    TimedOut,
    Aborted,
    NameNotResolved,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Mock(MockSpec),
    Sequence { steps: Vec<MockSpec>, after_last: AfterLast },
    /// Held for your answer (`network respond` or `fail`). A stream (EventSource, WebSocket) is held at
    /// its handshake: answered 200, you `send` its messages and `close` it.
    Hold,
    Passthrough,
    Fail { reason: FailReason },
    /// Pass through and keep the exchange in the bundle's `cassettes/<cassette>.jsonl`.
    Record { cassette: String },
    /// An EventSource or WebSocket stream the platform scripts, timed from the instant the page opened
    /// it: each message `at_ms` after, the server's close (if any) at `close_at_ms`.
    StreamMock {
        #[serde(default)]
        handshake: Handshake,
        #[serde(default)]
        messages: Vec<StreamMessage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        close_at_ms: Option<u64>,
    },
    /// Answer from a cassette file, each request's responses in recorded order; then `after_last`.
    Replay {
        cassette: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        latency_ms: Option<u64>,
        #[serde(default = "fail_after_last")]
        after_last: AfterLast,
    },
}

/// A stream's handshake: `status` 0 is success (`101` WebSocket, `200` Server-Sent Events); another fails
/// the stream.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handshake {
    #[serde(default)]
    pub status: u16,
}

/// One scripted message: its data, `at_ms` after the handshake, and for Server-Sent Events its event
/// name and id.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamMessage {
    pub at_ms: u64,
    pub data: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

fn fail_after_last() -> AfterLast {
    AfterLast::Fail
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: RuleId,
    #[serde(rename = "match")]
    pub matcher: Matcher,
    pub action: Action,
}

/// An ordered edit to caller-owned rules; generated launch defaults are not editable.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuleEdit {
    Add { rule: Box<Rule>, before: Option<RuleId> },
    Remove { id: RuleId },
}

pub struct RequestView<'a> {
    pub method: &'a str,
    pub url: &'a str,
    /// Header names in any case; matching lowercases both sides.
    pub headers: &'a BTreeMap<String, String>,
    pub body: Option<&'a serde_json::Value>,
}

impl Matcher {
    fn matches_route(&self, method: &str, url: &str) -> bool {
        self.method.as_ref().is_none_or(|m| m.eq_ignore_ascii_case(method)) && self.url.matches(url)
    }

    fn matches(&self, r: &RequestView) -> bool {
        if !self.matches_route(r.method, r.url) {
            return false;
        }
        if let Some(want) = &self.headers {
            let have: BTreeMap<String, &String> = r.headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect();
            for (k, v) in want {
                if have.get(&k.to_ascii_lowercase()).map(|x| x.as_str()) != Some(v.as_str()) {
                    return false;
                }
            }
        }
        if let Some(want) = &self.body {
            let Some(body) = r.body else { return false };
            for (pointer, v) in want {
                if body.pointer(pointer) != Some(v) {
                    return false;
                }
            }
        }
        true
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    #[error("invalid rule set: {0}")]
    Json(#[from] serde_json::Error),
    #[error("duplicate rule id {0}")]
    DuplicateId(RuleId),
    #[error("sequence rule {0} has no steps")]
    EmptySequence(RuleId),
    #[error("not an origin (scheme://host[:port]): {0:?}")]
    BadOrigin(String),
    #[error("rule {rule}: {reason}")]
    BadMock { rule: RuleId, reason: String },
}

impl MockSpec {
    /// A response the engine can deliver: an HTTP status and a latency representable in microseconds.
    pub fn validate(&self) -> Result<(), String> {
        if !(100..=599).contains(&self.status) {
            return Err(format!("status {} is not an HTTP status (100–599)", self.status));
        }
        if self.body_base64.is_some() && !self.body.is_empty() {
            return Err("give body or body_base64, not both".into());
        }
        if self.body_base64.as_deref().is_some_and(|b| crate::encode::unbase64(b).is_none()) {
            return Err("body_base64 is not base64".into());
        }
        if self.latency_ms > u64::MAX / 1000 {
            return Err(format!("latency {} ms is too large", self.latency_ms));
        }
        // RFC 9110: a field name is a token; a value has no line break or NUL. The engine rejects others
        // only when the response is delivered, long after the rule was accepted.
        let token = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
        for (name, value) in &self.headers {
            if name.is_empty() || !name.chars().all(token) {
                return Err(format!("header name {name:?} is not a token"));
            }
            if value.chars().any(|c| matches!(c, '\r' | '\n' | '\0')) {
                return Err(format!("header {name} has a line break or NUL in its value"));
            }
        }
        Ok(())
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RuleSet {
    pub rules: Vec<Rule>,
}

/// Durable receipt of one rule installation, independent of later observation changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleReceipt {
    pub revision: u64,
    pub sha256: String,
}

/// The effective rules observed at one stopped boundary, including platform fallbacks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSnapshot {
    pub boundary: crate::observation::ObservationBoundary,
    pub revision: u64,
    pub rules: RuleSet,
}

fn is_origin(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once("://") else { return false };
    !scheme.is_empty() && !rest.is_empty() && !rest.contains('/') && !rest.contains('*') && !rest.contains('?')
}

/// Check explicit network defaults before a launcher reserves or starts a runtime.
/// RFC 6901: empty (the whole document) or `/`-separated tokens, `~` only as `~0` or `~1`.
fn is_json_pointer(p: &str) -> bool {
    (p.is_empty() || p.starts_with('/')) && p.split('~').skip(1).all(|rest| rest.starts_with(['0', '1']))
}

pub fn validate_passthrough_origins(origins: &[String]) -> Result<(), RuleError> {
    for origin in origins {
        if !is_origin(origin) {
            return Err(RuleError::BadOrigin(origin.clone()));
        }
    }
    Ok(())
}

impl RuleSet {
    pub fn from_json(text: &str) -> Result<RuleSet, RuleError> {
        let set: RuleSet = serde_json::from_str(text)?;
        set.validate()?;
        Ok(set)
    }

    pub fn validate(&self) -> Result<(), RuleError> {
        let mut seen = HashSet::new();
        for r in &self.rules {
            if !seen.insert(r.id.clone()) {
                return Err(RuleError::DuplicateId(r.id.clone()));
            }
            if let Action::Sequence { steps, .. } = &r.action
                && steps.is_empty()
            {
                return Err(RuleError::EmptySequence(r.id.clone()));
            }
            // A key that is not a pointer would never match: refuse it rather than install a rule that
            // silently passes every request on.
            if let Some(key) = r.matcher.body.iter().flat_map(|b| b.keys()).find(|k| !is_json_pointer(k)) {
                let reason = format!("body condition {key:?} is not a JSON Pointer: write \"/{}\" for a top-level field", key.replace('~', "~0").replace('/', "~1"));
                return Err(RuleError::BadMock { rule: r.id.clone(), reason });
            }
            if let Action::Record { cassette } = &r.action
                && (cassette.is_empty() || !cassette.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)))
            {
                return Err(RuleError::BadMock { rule: r.id.clone(), reason: format!("cassette name {cassette:?} is not letters, digits, - and _") });
            }
            let specs: Vec<&MockSpec> = match &r.action {
                Action::Mock(m) => vec![m],
                Action::Sequence { steps, .. } => steps.iter().collect(),
                _ => vec![],
            };
            for spec in specs {
                spec.validate().map_err(|reason| RuleError::BadMock { rule: r.id.clone(), reason })?;
            }
        }
        Ok(())
    }

    /// The caller's rules followed by explicit defaults: one pass-through rule per origin, then a final
    /// rule that blocks everything else.
    pub fn effective(caller: RuleSet, passthrough_origins: &[String]) -> Result<RuleSet, RuleError> {
        validate_passthrough_origins(passthrough_origins)?;
        let mut rules = caller.rules;
        for (n, origin) in passthrough_origins.iter().enumerate() {
            rules.push(Rule {
                id: RuleId::new(format!("default-passthrough-{n}")).expect("valid id"),
                matcher: Matcher { method: None, url: UrlPattern(format!("{origin}/*")), headers: None, body: None },
                action: Action::Passthrough,
            });
        }
        rules.push(Rule {
            id: RuleId::new("default-block").expect("valid id"),
            matcher: Matcher { method: None, url: UrlPattern("*".into()), headers: None, body: None },
            action: Action::Fail { reason: FailReason::Aborted },
        });
        let set = RuleSet { rules };
        set.validate()?;
        Ok(set)
    }

    pub fn first_match(&self, request: &RequestView) -> Option<&Rule> {
        self.rules.iter().find(|r| r.matcher.matches(request))
    }

    /// The first rule whose method and URL match, header and body conditions aside: a CORS preflight
    /// carries neither the headers nor the body of the request it asks about.
    pub fn first_route_match(&self, method: &str, url: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.matcher.matches_route(method, url))
    }

    pub fn sha256(&self) -> String {
        sha256_hex(&canonical_of(self))
    }
}

/// Consumption positions of sequence rules; persisted control state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequenceCursors {
    positions: BTreeMap<RuleId, usize>,
}

impl SequenceCursors {
    /// The next step for `rule`, or `None` when the sequence is exhausted and `after_last` is `Fail`.
    pub fn next(&mut self, rule: &RuleId, steps: &[MockSpec], after_last: AfterLast) -> Option<MockSpec> {
        let pos = self.positions.entry(rule.clone()).or_insert(0);
        let step = match steps.get(*pos) {
            Some(s) => s.clone(),
            None => match after_last {
                AfterLast::RepeatLast => steps.last()?.clone(),
                AfterLast::Fail => return None,
            },
        };
        *pos += 1;
        Some(step)
    }

    /// The positions of the rules `next` installs unchanged from `previous`. A new, changed or removed
    /// rule starts its sequence again; an unchanged one continues where it was.
    pub fn carried(&self, previous: &RuleSet, next: &RuleSet) -> SequenceCursors {
        SequenceCursors {
            positions: self.positions.iter().filter(|(id, _)| unchanged(previous, next, id)).map(|(id, p)| (id.clone(), *p)).collect(),
        }
    }
}

/// Whether rule `id` is installed in both sets with the same definition.
pub fn unchanged(previous: &RuleSet, next: &RuleSet, id: &RuleId) -> bool {
    match (previous.rules.iter().find(|r| &r.id == id), next.rules.iter().find(|r| &r.id == id)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// For each rule of the last of consecutive `installations`, the earliest revision from which it has
/// stayed installed unchanged: the requests that selected it since then make up its consumption.
pub fn installed_since(installations: &BTreeMap<u64, RuleSet>) -> BTreeMap<RuleId, u64> {
    let mut out = BTreeMap::new();
    let Some((&last, current)) = installations.last_key_value() else { return out };
    for rule in &current.rules {
        let mut since = last;
        for (&revision, set) in installations.range(..last).rev() {
            if !set.rules.iter().any(|r| r == rule) {
                break;
            }
            since = revision;
        }
        out.insert(rule.id.clone(), since);
    }
    out
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod tests;
