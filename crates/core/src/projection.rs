use crate::canonical::to_canonical_string;

fn strip_wall_time(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("wall_ms");
            for v in map.values_mut() {
                strip_wall_time(v);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_wall_time),
        _ => {}
    }
}

fn strip_native_identity(value: &mut serde_json::Value) {
    if value.get("kind").and_then(|v| v.as_str()) == Some("exception")
        && let Some(payload) = value.get_mut("payload").and_then(|v| v.as_object_mut())
    {
        payload.remove("native");
        payload.remove("receipt");
    }
    // Stream receipt IDs identify browser-owned publication evidence, not the logical stream,
    // call order, payload or remote delivery. Those behavior fields remain compared unchanged.
    if value.get("kind").and_then(|v| v.as_str()) == Some("stream")
        && let Some(payload) = value.get_mut("payload").and_then(|v| v.as_object_mut())
    {
        payload.remove("receipt");
    }
    if value.get("kind").and_then(|v| v.as_str()) == Some("context")
        && let Some(payload) = value.get_mut("payload").and_then(|v| v.as_object_mut())
    {
        payload.remove("target");
    }
    if value.get("kind").and_then(|v| v.as_str()) == Some("action")
        && let Some(pointer) = value.pointer_mut("/payload/pointer").and_then(|v| v.as_object_mut()) {
        pointer.remove("native");
    }
}

/// The `behavior` projection of one trace line.
pub fn behavior_line(line: &str) -> Result<String, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(line)?;
    strip_wall_time(&mut value);
    strip_native_identity(&mut value);
    Ok(to_canonical_string(&value))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    pub index: usize,
    pub left: Option<String>,
    pub right: Option<String>,
}

/// Who drove a run names its requests (and the server, the watches it adds for a `wait`): request ids, and
/// watch names the server made (`mcp-…`, a prefix agents cannot use), are numbered by first appearance, so
/// two runs that issued the same commands compare equal whatever ids they used. Only those fields: text a
/// page or an agent wrote is compared as it is.
#[derive(Default)]
struct ControllerIds(std::collections::BTreeMap<String, usize>);

impl ControllerIds {
    fn ordinal(&mut self, id: &str) -> String {
        let next = self.0.len() + 1;
        format!("<id {}>", self.0.entry(id.to_string()).or_insert(next))
    }

    fn normalize(&mut self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, v) in map.iter_mut() {
                    match v {
                        serde_json::Value::String(s)
                            if key == "request_id" || ((key == "name" || key == "watch") && s.starts_with(crate::ids::RequestId::SERVER_PREFIX)) =>
                        {
                            *s = self.ordinal(s);
                        }
                        _ => self.normalize(v),
                    }
                }
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(|v| self.normalize(v)),
            _ => {}
        }
    }
}

fn project(text: &str) -> Vec<String> {
    let mut ids = ControllerIds::default();
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| match serde_json::from_str::<serde_json::Value>(l) {
            Ok(mut value) => {
                strip_wall_time(&mut value);
                strip_native_identity(&mut value);
                ids.normalize(&mut value);
                to_canonical_string(&value)
            }
            Err(_) => format!("<unparsable> {l}"),
        })
        .collect()
}

/// Compares two behavior logs under the `behavior` projection; returns the first differing line.
pub fn compare_behavior(left: &str, right: &str) -> Result<(), Difference> {
    let a = project(left);
    let b = project(right);
    for i in 0..a.len().max(b.len()) {
        if a.get(i) != b.get(i) {
            return Err(Difference { index: i, left: a.get(i).cloned(), right: b.get(i).cloned() });
        }
    }
    Ok(())
}

/// Replaces each `from` with its `to` where `from` ends at a boundary — `/`, `?`, `#`, `"`, `\`, or the
/// end — so an origin `http://h:1` never rewrites a prefix of `http://h:10`.
pub fn rewrite(text: &str, rewrites: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (from, to) in rewrites {
        if from.is_empty() {
            continue;
        }
        let mut next = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(i) = rest.find(from.as_str()) {
            let after = &rest[i + from.len()..];
            next.push_str(&rest[..i]);
            if after.is_empty() || after.starts_with(['/', '?', '#', '"', '\\']) {
                next.push_str(to);
            } else {
                next.push_str(from);
            }
            rest = after;
        }
        next.push_str(rest);
        out = next;
    }
    out
}

/// The `replay` projection of a behavior log: `behavior` without controller events (who drove the run
/// is not what the page did) and without `seq`, which they shift. `rewrites` replaces each `from` text
/// with its `to` in every line before projecting — an origin a replay was launched at instead.
fn project_replay(text: &str, rewrites: &[(String, String)], recorded: &[(u64, String)]) -> Vec<String> {
    let values: Vec<Result<serde_json::Value, String>> =
        text.lines().filter(|l| !l.is_empty()).map(|l| rewrite(l, rewrites)).map(|l| serde_json::from_str(&l).map_err(|_| l)).collect();
    // Requests of `recorded` rules: the recorded run passed them through, its re-execution answers them
    // from that recording. The same exchange either way.
    let is_recorded = |v: &serde_json::Value| {
        let revision = v.pointer("/payload/rules_revision").and_then(|r| r.as_u64()).unwrap_or(0);
        v.pointer("/payload/rule").and_then(|r| r.as_str())
            .is_some_and(|rule| recorded.iter().any(|(selected, id)| *selected == revision && id == rule))
    };
    let ordinals: Vec<String> = values
        .iter()
        .filter_map(|v| v.as_ref().ok())
        .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some("request") && is_recorded(v))
        .filter_map(|v| v.pointer("/payload/id").and_then(|i| i.as_str()).map(str::to_string))
        .collect();
    values
        .into_iter()
        .filter_map(|v| match v {
            Ok(mut v) => {
                let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or_default().to_string();
                if kind == "controller" {
                    return None;
                }
                if kind == "taint"
                    && v.pointer("/payload/source").and_then(|s| s.as_str()) == Some("passthrough_content")
                    && v.pointer("/payload/detail").and_then(|d| d.as_str()).is_some_and(|d| ordinals.iter().any(|o| o == d))
                {
                    return None;
                }
                // Responses inherit the request's selected installation. A reused rule id in
                // another installation must not hide a changed source or cassette entry.
                if matches!(kind.as_str(), "request" | "response")
                    && v.pointer("/payload/id").and_then(|id| id.as_str()).is_some_and(|id| ordinals.iter().any(|o| o == id))
                    && let Some(p) = v.get_mut("payload").and_then(|p| p.as_object_mut())
                {
                    p.remove("entry");
                    if kind == "response" {
                        p.insert("source".into(), "recorded".into());
                    }
                }
                strip_wall_time(&mut v);
                strip_native_identity(&mut v);
                if let Some(m) = v.as_object_mut() {
                    m.remove("seq");
                }
                // An observer report's receipt number counts the reports before it; their order is the
                // lines' own, so a report arriving at another place (a message, below) shifts only this.
                if let Some(p) = v.get_mut("payload").and_then(|p| p.as_object_mut()) {
                    p.remove("rseq");
                    if let Some(o) = p.get_mut("observer").and_then(|o| o.as_object_mut()) {
                        o.remove("rseq");
                    }
                }
                // Trigger references address the original trace, whose sequence includes controller
                // events. Ordinal or URL/document together with virtual time preserve event identity.
                if kind == "clock"
                    && let Some(triggers) = v.pointer_mut("/payload/outcome/triggers").and_then(|v| v.as_array_mut())
                {
                    for trigger in triggers.iter_mut().filter_map(|t| t.as_object_mut()) {
                        trigger.remove("seq");
                    }
                }
                Some(to_canonical_string(&v))
            }
            Err(l) => Some(format!("<unparsable> {l}")),
        })
        .collect()
}

/// A projected line, with its instant and whether its place among that instant's other lines is wall
/// time's rather than the page's. A pass-through response ends when the network delivers its body, so
/// its record and `passthrough_content` taint fall before or after what the page did at the same instant
/// (a document's script starting a worker's load while the document's body still arrives). A message
/// another window posted has varied its place among an instant's records between runs, its sender
/// awaiting storage replies from another process.
struct Projected {
    t_us: Option<u64>,
    network_timed: bool,
    text: String,
}

fn network_timed(line: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return false };
    match v.get("kind").and_then(|k| k.as_str()) {
        Some("response") => v.pointer("/payload/source").and_then(|s| s.as_str()) == Some("passthrough"),
        Some("taint") => v.pointer("/payload/source").and_then(|s| s.as_str()) == Some("passthrough_content"),
        Some("message") => true,
        _ => false,
    }
}

/// The `replay` projection with each instant's network-timed lines after its other lines, in their own
/// order; every other line keeps its place.
fn project_ordered(text: &str, rewrites: &[(String, String)], recorded: &[(u64, String)]) -> Vec<Projected> {
    let lines: Vec<Projected> = project_replay(text, rewrites, recorded).into_iter().map(|text| {
        let t_us = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v.get("t_us").and_then(|t| t.as_u64()));
        Projected { t_us, network_timed: network_timed(&text), text }
    }).collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut group: Vec<Projected> = Vec::new();
    let flush = |group: &mut Vec<Projected>, out: &mut Vec<Projected>| {
        let (timed, page): (Vec<Projected>, Vec<Projected>) = group.drain(..).partition(|l| l.network_timed);
        out.extend(page);
        out.extend(timed);
    };
    for line in lines {
        if group.last().is_some_and(|l| l.t_us != line.t_us || l.t_us.is_none()) {
            flush(&mut group, &mut out);
        }
        group.push(line);
    }
    flush(&mut group, &mut out);
    out
}

/// Compares a recorded behavior log (`left`, rewritten) with its replay under the `replay` projection.
/// `recorded` names (installation revision, rule id) pairs whose exchanges the recorded run kept
/// in a cassette and its replay answers from it.
pub fn compare_replay(left: &str, right: &str, rewrites: &[(String, String)], recorded: &[(u64, String)]) -> Result<(), Difference> {
    let a = project_ordered(left, rewrites, recorded);
    let b = project_ordered(right, &[], recorded);
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).map(|l| &l.text), b.get(i).map(|l| &l.text));
        if x != y {
            return Err(Difference { index: i, left: x.cloned(), right: y.cloned() });
        }
    }
    Ok(())
}

/// Like `compare_replay`, for a replay that is still running: its projected log must equal the
/// beginning of the recorded one, whatever the recorded run did afterwards. The replay's last instant
/// may be unfinished: its page lines must begin the recorded instant's, and its network-timed lines
/// must be among the recorded instant's.
pub fn compare_replay_prefix(left: &str, right: &str, rewrites: &[(String, String)], recorded: &[(u64, String)]) -> Result<(), Difference> {
    let a = project_ordered(left, rewrites, recorded);
    let b = project_ordered(right, &[], recorded);
    let last = b.last().and_then(|l| l.t_us);
    let open = b.iter().rposition(|l| l.t_us != last || last.is_none()).map_or(0, |i| i + 1);
    let differ = |i: usize, y: Option<&Projected>| Err(Difference { index: i, left: a.get(i).map(|l| l.text.clone()), right: y.map(|l| l.text.clone()) });
    for (i, y) in b.iter().enumerate().take(open) {
        if a.get(i).map(|l| &l.text) != Some(&y.text) {
            return differ(i, Some(y));
        }
    }
    let recorded_instant: Vec<&Projected> = a.iter().skip(open).take_while(|l| l.t_us == last && last.is_some()).collect();
    let recorded_page: Vec<&String> = recorded_instant.iter().filter(|l| !l.network_timed).map(|l| &l.text).collect();
    let mut recorded_timed: Vec<&String> = recorded_instant.iter().filter(|l| l.network_timed).map(|l| &l.text).collect();
    let mut page = 0;
    for (i, y) in b.iter().enumerate().skip(open) {
        if y.network_timed {
            match recorded_timed.iter().position(|t| **t == y.text) {
                Some(found) => { recorded_timed.remove(found); }
                None => return differ(i, Some(y)),
            }
        } else {
            if recorded_page.get(page) != Some(&&y.text) {
                return differ(i, Some(y));
            }
            page += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "projection_tests.rs"]
mod tests;
