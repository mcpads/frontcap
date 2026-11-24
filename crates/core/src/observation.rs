//! Results of observing a page: accessibility snapshots with node references, diffs, and queries.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One stopped occurrence; revisions tell effects at one virtual instant apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ObservationBoundary {
    pub launch_id: crate::ids::LaunchId,
    pub state_revision: u64,
    pub t_us: crate::time::VirtualTime,
    pub context_id: String,
    pub document_id: String,
}

impl ObservationBoundary {
    /// What differs from an `earlier` boundary, field by field (a consumer could not tell which changed).
    pub fn changes_from(&self, earlier: &ObservationBoundary) -> String {
        let mut changes = Vec::new();
        let mut field = |name: &str, was: String, now: String| if was != now { changes.push(format!("{name} {was} → {now}")) };
        field("launch_id", earlier.launch_id.to_string(), self.launch_id.to_string());
        field("context_id", earlier.context_id.clone(), self.context_id.clone());
        field("document_id", earlier.document_id.clone(), self.document_id.clone());
        field("t_us", earlier.t_us.0.to_string(), self.t_us.0.to_string());
        field("state_revision", earlier.state_revision.to_string(), self.state_revision.to_string());
        if changes.is_empty() { "unchanged".into() } else { changes.join(", ") }
    }
}

/// A page context of a generation: the launch page (`page`) or a window a context opened (`popup-1`…).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextEntry {
    pub id: String,
    /// The context that opened it; none for the launch page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opener: Option<String>,
    /// Its current document's URL, or once closed, its last document's.
    pub url: String,
    pub open: bool,
    /// Opened without an opener (`noopener`, a `target=_blank` link): its page's `window.opener` is null.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_opener: bool,
    /// The virtual instant it closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_us: Option<crate::time::VirtualTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationRelation {
    Current,
    Pending,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenePresentation {
    pub relation: PresentationRelation,
    pub capture: Option<Captured>,
    pub image: Option<ImageAnchor>,
}

/// An immutable image's mapping to its source viewport, retained by the recorder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAnchor {
    pub id: String,
    pub boundary: ObservationBoundary,
    pub capture: Captured,
    pub pixel_size: [u32; 2],
    pub viewport_size: [u32; 2],
    /// Source-image pixel rectangle; permits successive crops without losing the transform.
    pub clip: [u32; 4],
    pub source_size: [u32; 2],
    pub sha256: String,
}

/// Structural and visual evidence acquired under a single recorder lock. Coverage is explicit: a
/// coherent document read is not proof that every media surface or execution agent is controlled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub boundary: ObservationBoundary,
    pub snapshot: Snapshot,
    pub presentation: ScenePresentation,
    pub engine_revision: String,
    pub coverage_gaps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClickAssessment {
    pub ready: bool,
    pub point: Option<[f64; 2]>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inspection {
    pub boundary: ObservationBoundary,
    pub r#ref: String,
    pub values: BTreeMap<String, serde_json::Value>,
    /// Top-viewport CSS coordinates of the border quad, preserving transforms.
    pub border_quad: Option<Vec<f64>>,
    /// The same candidate point and checks used by a native click, without dispatching it.
    pub click: ClickAssessment,
}

/// A native viewport hit at a checked logical boundary. Coordinates are CSS pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Located {
    pub point: [i32; 2],
    pub target: Inspection,
    pub tag: String,
    /// True for a canvas surface; its internal visual regions have no DOM references.
    pub canvas: bool,
    /// Composed ancestry acquired with this hit. Older recordings did not acquire a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_path: Option<HitPath>,
}

/// From the hit node outward to the selected context's top document. An absent omission means that
/// the top document was reached; a missing `Located::hit_path` carries no ancestry evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HitPath {
    pub nodes: Vec<HitPathNode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omission: Option<HitPathOmission>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HitPathNode {
    pub kind: HitPathNodeKind,
    pub name: String,
    /// Element and text references use the same resolver as inspection, input and watches. Document
    /// and shadow-root entries are boundaries, not actionable element references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    /// How the preceding, more deeply nested entry reaches this one; `hit` is the first entry only.
    pub via: HitPathRelation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HitPathNodeKind { Element, Text, Document, ShadowRoot, Other }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HitPathRelation { Hit, Parent, AssignedSlot, ShadowHost, FrameOwner }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum HitPathOmission {
    DepthLimit { limit: u32 },
    InaccessibleBoundary { detail: String },
    Detached,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotNode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<SnapshotGeometry>,
    pub r#ref: String,
    pub role: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub depth: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<String>,
    /// This node stands for `count` siblings shaped like the ones before it, itself the first, and
    /// their contents; `unfold` lists them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folded: Option<Folded>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folded {
    pub count: u32,
    /// Their parent, the scope to list them in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Siblings in a run of this many with one shape are folded; the first [`FOLD_KEEP`] stay.
pub const FOLD_AT: usize = 6;
pub const FOLD_KEEP: usize = 3;

/// `nodes` (depth-first, with depths) with each run of at least [`FOLD_AT`] sibling subtrees of one
/// shape (their roles and nesting, not their text) kept to its first [`FOLD_KEEP`] and a folded node.
pub fn fold_repeats(nodes: &[SnapshotNode]) -> Vec<SnapshotNode> {
    fold(nodes, |_| false, None)
}

/// As [`fold_repeats`], but a run with siblings `on_screen` names (by root index) keeps its first
/// [`FOLD_KEEP`] of those instead of its first: each stretch left out before and after is one folded node.
pub fn fold_repeats_keeping(nodes: &[SnapshotNode], on_screen: impl Fn(usize) -> bool) -> Vec<SnapshotNode> {
    fold(nodes, on_screen, None)
}

/// The root indices of each run [`fold_repeats`] folds, run by run in document order.
pub fn fold_candidates(nodes: &[SnapshotNode]) -> Vec<Vec<usize>> {
    let mut runs = Vec::new();
    fold(nodes, |_| false, Some(&mut runs));
    runs
}

/// The page decides how deep the tree is, so the walk keeps its own stack and finds every subtree's
/// end in one pass.
fn fold(nodes: &[SnapshotNode], on_screen: impl Fn(usize) -> bool, mut candidates: Option<&mut Vec<Vec<usize>>>) -> Vec<SnapshotNode> {
    enum Task {
        /// The siblings in `nodes[start..end]` with their subtrees, under the parent at that index.
        Level { start: usize, end: usize, parent: Option<usize> },
        Emit(usize),
        Fold { at: usize, count: usize, parent: Option<usize> },
    }
    // `ends[i]`: one past the last node of the subtree rooted at `i`.
    let mut ends = vec![nodes.len(); nodes.len()];
    let mut open: Vec<usize> = Vec::new();
    for (j, node) in nodes.iter().enumerate() {
        while open.last().is_some_and(|&i| nodes[i].depth >= node.depth) {
            ends[open.pop().expect("an open subtree")] = j;
        }
        open.push(j);
    }
    let subtree = |i: usize| &nodes[i..ends[i]];
    let mut out = Vec::with_capacity(nodes.len().min(4096));
    let mut tasks = vec![Task::Level { start: 0, end: nodes.len(), parent: None }];
    while let Some(task) = tasks.pop() {
        let (start, end, parent) = match task {
            Task::Emit(i) => {
                out.push(nodes[i].clone());
                continue;
            }
            Task::Fold { at, count, parent } => {
                let mut marker = nodes[at].clone();
                marker.folded = Some(Folded { count: count as u32, scope: parent.map(|p| nodes[p].r#ref.clone()) });
                out.push(marker);
                continue;
            }
            Task::Level { start, end, parent } => (start, end, parent),
        };
        let mut children = Vec::new();
        let mut i = start;
        while i < end {
            children.push(i);
            i = ends[i];
        }
        // In document order; pushed reversed so the first is done first.
        let mut planned = Vec::new();
        let kept = |i: usize, planned: &mut Vec<Task>| {
            planned.push(Task::Emit(i));
            planned.push(Task::Level { start: i + 1, end: ends[i], parent: Some(i) });
        };
        let mut k = 0;
        while k < children.len() {
            let first = subtree(children[k]);
            let run = if children.len() - k < FOLD_AT { 1 } else { children[k..].iter().take_while(|&&c| same_shape(subtree(c), first)).count() };
            if run >= FOLD_AT {
                let members = &children[k..k + run];
                if let Some(runs) = candidates.as_deref_mut() {
                    runs.push(members.to_vec());
                }
                let mut shown: Vec<usize> = members.iter().copied().filter(|&c| on_screen(c)).take(FOLD_KEEP).collect();
                if shown.is_empty() {
                    shown = members[..FOLD_KEEP].to_vec();
                }
                let mut stretch: Option<(usize, usize)> = None;
                for &child in members {
                    if shown.contains(&child) {
                        if let Some((at, count)) = stretch.take() { planned.push(Task::Fold { at, count, parent }); }
                        kept(child, &mut planned);
                    } else {
                        stretch = Some(stretch.map_or((child, 1), |(at, count)| (at, count + 1)));
                    }
                }
                if let Some((at, count)) = stretch { planned.push(Task::Fold { at, count, parent }); }
            } else {
                for &child in &children[k..k + run] {
                    kept(child, &mut planned);
                }
            }
            k += run;
        }
        tasks.extend(planned.into_iter().rev());
    }
    out
}

/// Two subtrees with the same roles at the same relative depths.
fn same_shape(a: &[SnapshotNode], b: &[SnapshotNode]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.role == y.role && x.depth - a[0].depth == y.depth - b[0].depth)
}

/// A list of changed or added nodes with each run of at least [`FOLD_AT`] consecutive nodes of one
/// role and depth kept to its first [`FOLD_KEEP`] and a folded node.
pub fn fold_runs(nodes: Vec<SnapshotNode>) -> Vec<SnapshotNode> {
    let mut out = Vec::with_capacity(nodes.len().min(4096));
    let mut k = 0;
    while k < nodes.len() {
        let run = nodes[k..].iter().take_while(|n| n.role == nodes[k].role && n.depth == nodes[k].depth).count();
        if run >= FOLD_AT {
            out.extend(nodes[k..k + FOLD_KEEP].iter().cloned());
            let mut marker = nodes[k + FOLD_KEEP].clone();
            marker.folded = Some(Folded { count: (run - FOLD_KEEP) as u32, scope: None });
            out.push(marker);
        } else {
            out.extend(nodes[k..k + run].iter().cloned());
        }
        k += run;
    }
    out
}


/// Requested layout evidence in top-viewport CSS coordinates, including ancestor transforms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SnapshotGeometry {
    Box { border_quad: [f64; 8] },
    NoBox,
}

/// An accessibility snapshot of the current document, with references.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub geometry: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<ObservationBoundary>,
    pub observation_id: u64,
    pub document: u64,
    pub nodes: Vec<SnapshotNode>,
    pub omitted: u32,
    /// The subtree and depth it was taken with; a diff against it uses the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<crate::predicate::Locator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
    /// `pending` when the page changed after its last frame: the screen does not show this yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<PresentationRelation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diff {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<ObservationBoundary>,
    pub observation_id: u64,
    pub since: u64,
    pub added: Vec<SnapshotNode>,
    pub removed: Vec<String>,
    pub changed: Vec<SnapshotNode>,
    /// Nodes re-rendered with the same content under a new reference: neither removed nor added.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaced: Vec<Replaced>,
    /// Entries left out of `added`, `removed`, `changed` and `replaced` beyond the size limit.
    pub omitted: u32,
}

/// A node whose element was replaced by an identical one: its old reference and its new one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replaced {
    pub was: String,
    pub r#ref: String,
}

/// A property `query` reads: `text`, `value`, `visible`, `bbox`, `attr:<name>`, `style:<property>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prop {
    Text,
    Value,
    Visible,
    Bbox,
    Interaction,
    Scroll,
    Selection,
    Attr(String),
    Style(String),
}

impl Prop {
    pub fn parse(s: &str) -> Option<Prop> {
        Some(match s {
            "text" => Prop::Text,
            "value" => Prop::Value,
            "visible" => Prop::Visible,
            "bbox" => Prop::Bbox,
            "interaction" => Prop::Interaction,
            "scroll" => Prop::Scroll,
            "selection" => Prop::Selection,
            _ => match s.split_once(':') {
                Some(("attr", n)) if !n.is_empty() => Prop::Attr(n.into()),
                Some(("style", n)) if !n.is_empty() => Prop::Style(n.into()),
                _ => return None,
            },
        })
    }

    pub fn key(&self) -> String {
        match self {
            Prop::Text => "text".into(),
            Prop::Value => "value".into(),
            Prop::Visible => "visible".into(),
            Prop::Bbox => "bbox".into(),
            Prop::Interaction => "interaction".into(),
            Prop::Scroll => "scroll".into(),
            Prop::Selection => "selection".into(),
            Prop::Attr(n) => format!("attr:{n}"),
            Prop::Style(n) => format!("style:{n}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    pub r#ref: String,
    pub values: BTreeMap<String, serde_json::Value>,
    /// `pending` when the page changed after its last frame: the screen does not show this yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<PresentationRelation>,
}


/// A capture of the last rendered frame (time-model.md, Capture and time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Captured {
    /// The image, in the bundle.
    pub path: std::path::PathBuf,
    /// The frame's number in this generation.
    pub frame: u64,
    /// The virtual instant the frame shows, and the document it shows.
    pub t_us: crate::time::VirtualTime,
    pub document: u64,
    /// Whether the frame certainly shows the state at `t_us`. It does not when several states followed
    /// each other faster than the renderer draws (short segments while a request is held) and none of the
    /// earlier ones had a frame: the image may then show one of them.
    pub exact: bool,
    /// Whether it is exact, virtual time has not moved, and nothing changed the page (an action, a navigation, a
    /// response, a dialog, a script) since the frame: it still shows the current state. Otherwise advance
    /// by one frame interval for a frame of the current state.
    pub current: bool,
    /// The instant the pixels were painted (frontcap's patched engine): `t_us` for a frame that is not
    /// current, earlier for one still current, so two captures of one paint share it (consumer runs: a
    /// later capture of an older paint read as time running backwards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub painted_at_us: Option<crate::time::VirtualTime>,
}

/// One recorded frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedFrame {
    pub frame: u64,
    pub t_us: crate::time::VirtualTime,
    pub document: u64,
    /// Relative to the bundle.
    pub file: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recording {
    pub frames: Vec<RecordedFrame>,
    /// Instants of presented renders whose frame could not be copied (frontcap's patched engine).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missed: Vec<crate::time::VirtualTime>,
}

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;
