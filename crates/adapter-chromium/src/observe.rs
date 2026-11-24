//! Snapshots: the accessibility tree pruned to what an agent acts on, and diffs between snapshots.
use std::collections::{BTreeMap, BTreeSet};

/// One accessibility node, as read from the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct AxInput {
    pub id: String,
    pub parent: Option<String>,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub states: Vec<String>,
    pub backend: Option<i64>,
    pub ignored: bool,
}

/// A node kept in a snapshot, before it is given a reference.
#[derive(Debug, Clone, PartialEq)]
pub struct Kept {
    pub backend: Option<i64>,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub depth: u32,
    pub states: Vec<String>,
}

pub use frontcap_core::observation::SnapshotNode;

const KEEP: &[&str] = &[
    "button", "link", "textbox", "searchbox", "checkbox", "radio", "combobox", "listbox", "option", "menuitem", "menuitemcheckbox",
    "menuitemradio", "tab", "switch", "slider", "spinbutton", "heading", "dialog", "alertdialog", "alert", "navigation", "main", "form",
    "list", "listitem", "table", "row", "cell", "columnheader", "rowheader", "img", "status",
];

/// One tree of the page's frames, each frame's tree under the node of the element that shows it (an
/// iframe is a leaf in its parent's tree). Node ids are only unique within a frame, so each frame's are
/// prefixed with its index. A frame whose element is not in its parent's tree stays a root of its own.
pub fn stitch(frames: Vec<(Option<i64>, Vec<AxInput>)>) -> Vec<AxInput> {
    let mut out: Vec<AxInput> = Vec::new();
    for (i, (owner, nodes)) in frames.into_iter().enumerate() {
        let at = owner.and_then(|b| out.iter().find(|n| n.backend == Some(b)).map(|n| n.id.clone()));
        out.extend(nodes.into_iter().map(|n| AxInput {
            id: format!("{i}:{}", n.id),
            parent: match n.parent {
                Some(p) => Some(format!("{i}:{p}")),
                None => at.clone(),
            },
            ..n
        }));
    }
    out
}

/// The kept nodes in document order, at most `max`, and how many more there were. `scope` starts at the
/// node with that backend id; `depth` limits nesting among kept nodes (0 = top level only). A text node
/// that only repeats part of its nearest kept ancestor's name (a button's label, the pieces of a heading
/// "To do (2)") is dropped, and text pieces next to each other under one parent are one text node.
pub fn prune(nodes: &[AxInput], scope: Option<i64>, depth: Option<u32>, max: u32) -> (Vec<Kept>, u32) {
    prune_with_text_merging(nodes, scope, depth, max, true)
}

/// Geometry snapshots keep text nodes separate: each reference's box must describe its own text.
pub fn prune_with_text_merging(nodes: &[AxInput], scope: Option<i64>, depth: Option<u32>, max: u32, merge_text: bool) -> (Vec<Kept>, u32) {
    let mut children: BTreeMap<&str, Vec<&AxInput>> = BTreeMap::new();
    for n in nodes {
        if let Some(p) = &n.parent {
            children.entry(p.as_str()).or_default().push(n);
        }
    }
    let roots: Vec<&AxInput> = match scope {
        Some(b) => nodes.iter().filter(|n| n.backend == Some(b)).take(1).collect(),
        None => nodes.iter().filter(|n| n.parent.is_none()).collect(),
    };
    let mut kept: Vec<Kept> = Vec::new();
    let mut total = 0u32;
    // The parent of the text node kept last, and its index: a following sibling piece joins it.
    let mut last_text: Option<(Option<String>, usize)> = None;
    // (node, depth of the next kept node, nearest kept ancestor's name)
    let mut stack: Vec<(&AxInput, u32, String)> = roots.into_iter().rev().map(|r| (r, 0, String::new())).collect();
    while let Some((n, d, ancestor)) = stack.pop() {
        let repeats = |text: &str| !ancestor.trim().is_empty() && ancestor.contains(text.trim());
        // A frame element is kept as the boundary of the document it shows, so a frame that shows nothing
        // (refused, failed or empty) is still seen; its document is not a repetition of its title.
        let frame = n.role == "Iframe";
        let keep = !n.ignored && (KEEP.contains(&n.role.as_str()) || frame || (n.role == "StaticText" && !n.name.trim().is_empty() && !repeats(&n.name)));
        let (next_depth, next_ancestor) = if keep {
            if depth.is_none_or(|limit| d <= limit) {
                total += 1;
                let text = n.role == "StaticText";
                let joins = merge_text && text && last_text.as_ref().is_some_and(|(parent, at)| *parent == n.parent && *at + 1 == kept.len());
                let mut placed = joins;
                if joins {
                    // The next piece of the same run of text: one node, as a reader sees it.
                    total -= 1;
                    if let Some(k) = kept.last_mut() {
                        k.name.push_str(&n.name);
                    }
                } else if (kept.len() as u32) < max {
                    let role = if text { "text".to_string() } else if frame { "iframe".to_string() } else { n.role.clone() };
                    kept.push(Kept { backend: n.backend, role, name: n.name.clone(), value: n.value.clone(), depth: d, states: n.states.clone() });
                    placed = true;
                }
                last_text = if text && placed { Some((n.parent.clone(), kept.len() - 1)) } else { None };
            }
            (d + 1, if frame { String::new() } else { n.name.clone() })
        } else {
            (d, ancestor)
        };
        if depth.is_some_and(|limit| next_depth > limit.saturating_add(1)) {
            continue;
        }
        if n.role == "InlineTextBox" {
            continue;
        }
        if let Some(cs) = children.get(n.id.as_str()) {
            for c in cs.iter().rev() {
                stack.push((c, next_depth, next_ancestor.clone()));
            }
        }
    }
    let omitted = total - kept.len() as u32;
    (kept, omitted)
}

/// Nodes added (in the new snapshot only), removed (references gone), and changed (same reference,
/// different accessibility, depth or requested geometry).
/// What changed between two snapshots' nodes, by reference.
#[derive(Debug, Default)]
pub struct NodeDiff {
    pub added: Vec<SnapshotNode>,
    pub removed: Vec<String>,
    pub changed: Vec<SnapshotNode>,
    /// A node that went and one that came with everything but its reference equal (a re-rendered
    /// element), paired in document order as `(old reference, new reference)` instead of being reported
    /// as removed and added.
    pub replaced: Vec<(String, String)>,
}

pub fn diff_nodes(before: &[SnapshotNode], after: &[SnapshotNode]) -> NodeDiff {
    let old: BTreeMap<&str, &SnapshotNode> = before.iter().map(|n| (n.r#ref.as_str(), n)).collect();
    let new: BTreeSet<&str> = after.iter().map(|n| n.r#ref.as_str()).collect();
    let content = |n: &SnapshotNode| SnapshotNode { r#ref: String::new(), ..n.clone() };
    let mut gone: Vec<(SnapshotNode, &str)> = before.iter().filter(|n| !new.contains(n.r#ref.as_str())).map(|n| (content(n), n.r#ref.as_str())).collect();
    let mut added = Vec::new();
    let mut changed = Vec::new();
    let mut replaced = Vec::new();
    for n in after {
        match old.get(n.r#ref.as_str()) {
            None => match gone.iter().position(|(c, _)| *c == content(n)) {
                Some(i) => replaced.push((gone.remove(i).1.to_string(), n.r#ref.clone())),
                None => added.push(n.clone()),
            },
            Some(o) if *o != n => changed.push(n.clone()),
            Some(_) => {}
        }
    }
    let removed = gone.into_iter().map(|(_, r)| r.to_string()).collect();
    NodeDiff { added, removed, changed, replaced }
}

#[cfg(test)]
#[path = "observe_tests.rs"]
mod tests;
