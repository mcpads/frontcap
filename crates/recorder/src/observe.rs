//! Observation read from the trace: what a step did, and a watch's samples.
use frontcap_core::event::{BehaviorEvent, EventKind, ResponseSource};
use frontcap_core::ids::WatchName;
use frontcap_core::time::VirtualTime;
pub use frontcap_recorder_protocol::{ConsoleCounts, ConsoleEntry, ConsoleFilter, Effects, EffectsOmitted, HistorySample, NetworkEntry, NetworkFilter};
use std::collections::BTreeMap;

/// Requests issued, navigations and console output inside the selected trace window.
/// `holds` identifies hold rules for a step whose historical request inventory is unavailable.
/// Responses outside the window never rewrite that step's result.
/// Each list is bounded (`bound`): the histories hold every entry.
pub fn effects_with(events: &[BehaviorEvent], after_seq: u64, before_seq: Option<u64>, holds: impl Fn(&str) -> bool) -> Effects {
    let mut out = effects_classified(events, after_seq, before_seq, |request| holds(request.rule.as_ref().map_or("none", |r| r.as_str())));
    bound(&mut out);
    out
}

fn effects_classified(events: &[BehaviorEvent], after_seq: u64, before_seq: Option<u64>, held: impl Fn(&frontcap_core::event::RequestRecord) -> bool) -> Effects {
    // Responses count only inside the same window: a later answer does not rewrite this step's effects.
    let mut outcomes = BTreeMap::new();
    // Why the platform could not answer a request as its rule asked, by ordinal (`req-3: …`).
    let mut refusals = BTreeMap::new();
    for e in events.iter().filter(|e| before_seq.is_none_or(|b| e.seq < b)) {
        match &e.kind {
            EventKind::Response(r) => {
                outcomes.insert(r.id.clone(), r.clone());
            }
            EventKind::Taint(t) if matches!(t.source, frontcap_core::event::TaintSource::UnsupportedStream | frontcap_core::event::TaintSource::CassetteMiss) => {
                if let Some((ordinal, why)) = t.detail.split_once(": ") {
                    refusals.insert(ordinal.to_string(), why.to_string());
                }
            }
            _ => {}
        }
    }
    let mut out = Effects::default();
    for e in events.iter().filter(|e| e.seq >= after_seq && before_seq.is_none_or(|b| e.seq < b)) {
        match &e.kind {
            EventKind::Action(a) => {
                if let Some(events) = &a.received_events {
                    out.input.push(frontcap_recorder_protocol::InputEffect {
                        request_id: a.request_id.clone(), action: a.action.clone(), context: a.context.clone(), events: events.clone(),
                    });
                }
            }
            EventKind::Request(r) => {
                let rule = r.rule.as_ref().map_or("none", |id| id.as_str());
                let outcome = match outcomes.get(&r.id) {
                    None if held(r) => format!("held(rule={rule})"),
                    None => format!("pending(rule={rule})"),
                    Some(resp) => match resp.source {
                        ResponseSource::Mock => format!("mock(rule={rule}) {}", resp.status),
                        ResponseSource::Replay => format!("replay(rule={rule}) {}", resp.status),
                        ResponseSource::Passthrough => format!("passthrough(rule={rule}) {}", resp.status),
                        ResponseSource::Agent => format!("agent(rule={rule}) {}", resp.status),
                        // The platform's own answer for a mocked route: a preflight is matched by method and
                        // URL only, so the rule it met need not be the one its request then meets.
                        ResponseSource::Preflight => format!("preflight {}", resp.status),
                        ResponseSource::Fail => format!("fail(rule={rule})"),
                    },
                };
                let from = r.context.as_ref().map_or(String::new(), |c| format!("{c}: "));
                let outcome = match refusals.get(&r.id) {
                    Some(why) => format!("{outcome} ({why})"),
                    None => outcome,
                };
                out.requests.push(format!("{from}{} {} → {outcome}", r.method, r.url));
                if rule == "default-block" {
                    out.unmatched_requests.push(format!("{from}{} {}", r.method, r.url));
                }
            }
            EventKind::Navigation(n) => out.navigations.push(match &n.context {
                Some(c) => format!("{c}: {}", n.url),
                None => n.url.clone(),
            }),
            EventKind::Dialog(d) => {
                let kind = serde_json::to_value(d.kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                let from = d.context.as_ref().map_or(String::new(), |c| format!("{c}: "));
                out.dialogs.push(format!("{from}{kind} {:?} → {}", d.message, d.answer));
            }
            EventKind::Exception(x) => out.exceptions.push(match &x.context {
                Some(c) => format!("{c}: {}", x.text),
                None => x.text.clone(),
            }),
            EventKind::Context(c) => out.contexts.push(match (c.event, &c.opener) {
                (frontcap_core::event::ContextEvent::Opened, Some(opener)) if c.no_opener => format!("{} opened by {opener} without an opener", c.context),
                (frontcap_core::event::ContextEvent::Opened, Some(opener)) => format!("{} opened by {opener}", c.context),
                (frontcap_core::event::ContextEvent::Opened, None) => format!("{} opened", c.context),
                (frontcap_core::event::ContextEvent::Closed, _) => format!("{} closed", c.context),
            }),
            // A window the platform refused: the agent sees it was not opened rather than nothing.
            EventKind::Taint(t) if t.source == frontcap_core::event::TaintSource::NewWindow => out.contexts.push(format!("refused: {}", t.detail)),
            EventKind::Focus(f) => {
                let from = f.context.as_ref().map_or(String::new(), |c| format!("{c}: "));
                out.focus.push(format!("{from}{} {}", if f.focused { "focus" } else { "blur" }, f.target));
            }
            EventKind::Message(m) => {
                let to = m.context.as_ref().map_or(String::new(), |c| format!("{c}: "));
                out.messages.push(format!("{to}from {} {}", m.origin, m.data));
            }
            EventKind::Console(c) => {
                match c.level.as_str() {
                    "error" | "assert" => out.console.error += 1,
                    "warning" | "warn" => out.console.warn += 1,
                    _ => continue,
                }
                out.console_messages.push(console_entry(e.t_us, c));
            },
            _ => {}
        }
    }
    let mut blocked: Vec<&str> = Vec::new();
    for url in out.unmatched_requests.iter().filter_map(|r| r.rsplit(' ').next()) {
        if let Some(o) = origin(url).filter(|o| !blocked.contains(o)) {
            blocked.push(o);
        }
    }
    if !blocked.is_empty() {
        out.unmatched_next = Some(format!("Blocked: let {} through (launch passthrough_origins, or a passthrough rule), or answer with a rule", blocked.join(", ")));
    }
    out
}

/// Repeated console messages as one entry with their count, and each list to its first
/// [`Effects::ENTRIES`], the rest counted.
fn bound(out: &mut Effects) {
    let mut merged: Vec<ConsoleEntry> = Vec::new();
    for entry in std::mem::take(&mut out.console_messages) {
        let same = |m: &&mut ConsoleEntry| m.level == entry.level && m.text == entry.text && m.source == entry.source && m.context == entry.context;
        match merged.iter_mut().find(same) {
            Some(m) => m.count = Some(m.count.unwrap_or(1) + 1),
            None => merged.push(entry),
        }
    }
    out.console_messages = merged;
    let mut entries = BTreeMap::new();
    let mut keep = |name: &str, len: usize| -> usize {
        if len > Effects::ENTRIES {
            entries.insert(name.to_string(), (len - Effects::ENTRIES) as u32);
        }
        len.min(Effects::ENTRIES)
    };
    out.requests.truncate(keep("requests", out.requests.len()));
    out.unmatched_requests.truncate(keep("unmatched_requests", out.unmatched_requests.len()));
    out.navigations.truncate(keep("navigations", out.navigations.len()));
    out.console_messages.truncate(keep("console_messages", out.console_messages.len()));
    out.dialogs.truncate(keep("dialogs", out.dialogs.len()));
    out.exceptions.truncate(keep("exceptions", out.exceptions.len()));
    out.contexts.truncate(keep("contexts", out.contexts.len()));
    out.focus.truncate(keep("focus", out.focus.len()));
    out.messages.truncate(keep("messages", out.messages.len()));
    if !entries.is_empty() {
        let mut histories: Vec<&str> = entries.keys().map(|list| match list.as_str() {
            "requests" | "unmatched_requests" => "debug requests",
            "console_messages" | "exceptions" => "debug console",
            _ => "debug events",
        }).collect();
        histories.sort_unstable();
        histories.dedup();
        out.omitted = Some(EffectsOmitted { entries, next: format!("{} list every entry", histories.join(", ")) });
    }
}

/// `scheme://host[:port]` of a URL.
fn origin(url: &str) -> Option<&str> {
    let after_scheme = url.find("://")? + 3;
    Some(&url[..url[after_scheme..].find(['/', '?', '#']).map_or(url.len(), |end| after_scheme + end)])
}

fn in_range(t: u64, since: Option<u64>, until: Option<u64>) -> bool {
    since.is_none_or(|s| t >= s) && until.is_none_or(|u| t <= u)
}

fn console_entry(t_us: VirtualTime, c: &frontcap_core::event::ConsoleRecord) -> ConsoleEntry {
    ConsoleEntry { t_us, level: c.level.clone(), text: c.text.clone(), source: c.source.clone(), context: c.context.clone(), count: None }
}

/// Console output and uncaught exceptions (level `exception`) matching the filter, in order.
pub fn console(events: &[BehaviorEvent], f: &ConsoleFilter) -> Vec<ConsoleEntry> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Console(c) => Some(console_entry(e.t_us, c)),
            EventKind::Exception(x) => Some(ConsoleEntry { t_us: e.t_us, level: "exception".into(), text: x.text.clone(), source: None, context: x.context.clone(), count: None }),
            _ => None,
        })
        .filter(|entry| {
            in_range(entry.t_us.0, f.since_us, f.until_us)
                && f.level.as_ref().is_none_or(|l| l == &entry.level)
                && f.text_contains.as_ref().is_none_or(|s| entry.text.contains(s.as_str()))
        })
        .collect()
}

/// Requests issued in the range, described by their outcome, matching the filter.
pub fn network(events: &[BehaviorEvent], f: &NetworkFilter, held: &std::collections::BTreeSet<String>) -> Vec<NetworkEntry> {
    // The selected rule cannot tell whether a particular request is currently held: a delayed
    // mock may still be pending, and an explicit interception stop can hold a non-hold rule.
    let described = effects_classified(events, 0, None, |request| held.contains(&request.id)).requests;
    let answered: BTreeMap<&str, VirtualTime> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Response(r) => Some((r.id.as_str(), e.t_us)),
            _ => None,
        })
        .collect();
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Request(r) => Some((e.t_us, r, answered.get(r.id.as_str()).copied())),
            _ => None,
        })
        .zip(described)
        .filter(|((t, r, _), _)| in_range(t.0, f.since_us, f.until_us) && f.url_contains.as_ref().is_none_or(|s| r.url.contains(s.as_str())))
        .map(|((t_us, r, answered_us), request)| {
            let body = r.body.as_ref();
            NetworkEntry {
                ordinal: r.id.clone(), t_us, answered_us, request,
                body: body.and_then(|b| b.text.clone()),
                body_bytes: body.and_then(|b| b.bytes),
                body_unavailable: body.is_some_and(|b| b.unavailable),
            }
        })
        .collect()
}

pub fn history(events: &[BehaviorEvent], name: &WatchName) -> Vec<HistorySample> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Watch(s) if &s.name == name => Some(HistorySample { seq: e.seq, t_us: e.t_us, value: s.value, point: s.point }),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
#[path = "observe_tests.rs"]
mod tests;
