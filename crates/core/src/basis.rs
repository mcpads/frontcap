//! Saved starting points: an
//! immutable, content-identified basis and the reconstruction recipe it holds. A basis names the method
//! that restores it; a child is never constructed with another method than the one requested.
use crate::ids::{RunId, SavedId, WatchName};
use crate::intervention::StateChange;
use crate::predicate::{Locator, Predicate};
use crate::profile::DeterminismProfile;
use crate::rules::RuleSet;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// How a saved starting point is restored. `checkpoint` is saved where bootstrap's `basis_kinds` lists it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BasisKind {
    /// Build declared state, reach a barrier and verify named predicates; hidden runtime state is not restored.
    Recipe,
    /// Repeat a recorded command prefix: recorded exchanges answer from cassettes, pass-through reaches the
    /// app as it is now, and every command must answer as recorded.
    Replay,
    /// Restore continued execution, including closures, timers and transport state: a process image of a
    /// live generation's browser.
    Checkpoint,
}

impl BasisKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BasisKind::Recipe => "recipe",
            BasisKind::Replay => "replay",
            BasisKind::Checkpoint => "checkpoint",
        }
    }
}

/// How construction reaches the app's starting point once the browser-owned state is installed.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Barrier {
    /// Stay at the start boundary, before the first application script.
    None,
    /// Progress until an idle window or the bound; either end is reported, neither proves readiness.
    Settle { idle_window_ms: u64, max_ms: u64 },
    /// Progress until the predicate holds; reaching the bound first fails construction.
    Until { predicate: Predicate, max_ms: u64 },
}

impl Default for Barrier {
    fn default() -> Barrier {
        Barrier::Settle { idle_window_ms: 250, max_ms: 10_000 }
    }
}

/// A named startup condition, kept as a watch in every child; a child is ready only when it holds.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verification {
    pub name: WatchName,
    pub predicate: Predicate,
}

/// Declarative construction of a starting point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub url: String,
    /// Effective launch profile. Its `host_class` is `unspecified`: each child records its own host.
    pub profile: DeterminismProfile,
    pub rules: RuleSet,
    pub passthrough_origins: Option<Vec<String>>,
    pub barrier: Barrier,
    /// Typed changes applied after the barrier.
    pub changes: Vec<StateChange>,
    /// Maximum virtual progress per app-provider write, further bounded by the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_ms: Option<u64>,
    pub verify: Vec<Verification>,
    /// The dialog rules each child answers by, set at its start boundary.
    #[serde(default, skip_serializing_if = "is_no_dialog_rules")]
    pub dialog_rules: crate::dialog::DialogRules,
}

fn is_no_dialog_rules(rules: &crate::dialog::DialogRules) -> bool {
    rules.0.is_empty()
}

/// Bounds shared with `state apply` and the progress operations.
pub const MAX_CHANGES: usize = 64;
pub const MAX_VERIFICATIONS: usize = 32;
pub const MAX_BARRIER_MS: u64 = 600_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecipeError {
    #[error("{0}")]
    Invalid(String),
}

fn invalid(message: impl Into<String>) -> RecipeError {
    RecipeError::Invalid(message.into())
}

fn locator_portable(l: &Locator, what: &str) -> Result<(), RecipeError> {
    if !l.refs().is_empty() {
        return Err(invalid(format!("{what} names a node reference; references belong to one document, so a recipe names nodes by role, text, test id or CSS")));
    }
    Ok(())
}

fn predicate_portable(p: &Predicate, what: &str) -> Result<(), RecipeError> {
    locator_portable(p.target(), what)

}

/// Checks a list of typed changes a construction applies in one batch.
pub fn validate_changes(changes: &[StateChange], what: &str) -> Result<(), RecipeError> {
    if changes.len() > MAX_CHANGES {
        return Err(invalid(format!("{what} has {} changes; at most {MAX_CHANGES}", changes.len())));
    }
    for (i, c) in changes.iter().enumerate() {
        locator_portable(&c.target, &format!("{what}[{i}]"))?;
        if !c.field.writable() {
            return Err(invalid(format!("{what}[{i}] names a read-only field")));
        }
    }
    Ok(())
}

impl Recipe {
    /// Static checks that do not need the launch validation of the recorder: portability of node
    /// names, finite bounds and unique verification names.
    pub fn validate(&self) -> Result<(), RecipeError> {
        if self.progress_ms.is_some_and(|ms| ms > 10_000) {
            return Err(invalid("progress_ms is at most 10000"));
        }
        self.rules.validate().map_err(|e| invalid(e.to_string()))?;
        crate::rules::validate_passthrough_origins(self.passthrough_origins.as_deref().unwrap_or_default()).map_err(|e| invalid(e.to_string()))?;
        match &self.barrier {
            Barrier::None => {}
            Barrier::Settle { idle_window_ms, max_ms } => {
                if *idle_window_ms == 0 || *max_ms == 0 || *max_ms > MAX_BARRIER_MS {
                    return Err(invalid(format!("barrier settle needs a nonzero idle window and 1..={MAX_BARRIER_MS} ms")));
                }
            }
            Barrier::Until { predicate, max_ms } => {
                if *max_ms == 0 || *max_ms > MAX_BARRIER_MS {
                    return Err(invalid(format!("barrier until needs 1..={MAX_BARRIER_MS} ms")));
                }
                predicate_portable(predicate, "barrier predicate")?;
            }
        }
        validate_changes(&self.changes, "changes")?;
        validate_verify(&self.verify)
    }
}

/// Named predicates a child must meet once ready: at most a few, uniquely named, portable.
pub fn validate_verify(verify: &[Verification]) -> Result<(), RecipeError> {
    if verify.len() > MAX_VERIFICATIONS {
        return Err(invalid(format!("verify has {} predicates; at most {MAX_VERIFICATIONS}", verify.len())));
    }
    let mut names = BTreeSet::new();
    for v in verify {
        if v.name.as_str().starts_with(crate::ids::RequestId::SERVER_PREFIX) {
            return Err(invalid(format!("verify name {} starts with the server's prefix", v.name)));
        }
        if !names.insert(v.name.as_str()) {
            return Err(invalid(format!("verify name {} is used twice", v.name)));
        }
        predicate_portable(&v.predicate, &format!("verify {}", v.name))?;
    }
    Ok(())
}

/// The finalized run a replay basis repeats. The digest of the run's manifest pins its inputs and
/// trace, since the manifest records every member's digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaySource {
    pub run: RunId,
    pub manifest_sha256: String,
    /// The commands of the recorded prefix.
    pub commands: usize,
    /// The prefix was copied from a live generation and is kept in the basis (`bundle/<run>`) rather
    /// than read from the workspace's runs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub kept: bool,
}

/// The process image a checkpoint basis restores, kept in the workspace's private images under the basis's
/// saved ID and pinned by the digest of its whole tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointSource {
    /// SHA-256 over every entry of the image's tree: its path, type, mode, owner and content.
    pub tree_sha256: String,
    /// The bytes of its files.
    pub bytes: u64,
}

/// The immutable content of a saved basis. Its canonical JSON identifies it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasisManifest {
    pub kind: BasisKind,
    /// The content of a `recipe` basis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<Recipe>,
    /// The content of a `replay` basis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<ReplaySource>,
    /// The content of a `checkpoint` basis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointSource>,
    /// Basis-relative file name → SHA-256 of a file the recipe names (a replay rule's cassette).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, String>,
    /// The live generation and boundary a recipe or checkpoint was saved from; none for an authored recipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<BasisOrigin>,
    /// What the saved basis does not restore of that generation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<String>,
    /// Named predicates each child of a replay or checkpoint basis must meet once ready; a recipe keeps
    /// its own in the recipe.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verify: Vec<Verification>,
}

/// The live generation, and its frozen boundary, a recipe was saved from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BasisOrigin {
    pub launch_id: crate::ids::LaunchId,
    pub run: RunId,
    pub boundary: crate::observation::ObservationBoundary,
}

/// What a recipe can reconstruct of a live generation, read at its frozen boundary without progress:
/// the top document's address, its windows with their local and session storage, the cookies, the
/// native storage partition, and the values of its writable app state providers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveState {
    pub boundary: crate::observation::ObservationBoundary,
    pub url: String,
    /// The launch window's current name.
    pub window_name: String,
    /// The other windows in creation order, closed ones included.
    pub contexts: Vec<crate::profile::InitialContext>,
    /// Every origin's local storage and each window's session storage, in native bytes.
    pub dom_storage: crate::dom_storage::DomStorageState,
    pub cookies: Vec<crate::intervention::InitialCookie>,
    /// Native cookie-store inventory, rather than cookies matching only the top URL.
    #[serde(default)]
    pub cookie_inventory_complete: bool,
    pub app: Vec<AppValue>,
    /// Exact policies from the native storage partition, including empty buckets.
    #[serde(default)]
    pub buckets: Vec<crate::storage::InitialBucket>,
    /// True only after complete native inventory and scoped capture succeeded. Older captures
    /// cover only the top document's default bucket and must not clear unrelated launch seeds.
    #[serde(default)]
    pub storage_inventory_complete: bool,
    #[serde(default)]
    pub storage_excluded: Vec<String>,
    /// IndexedDB content within the captured scope coverage.
    #[serde(default)]
    pub indexed_db: Vec<crate::indexed_db::InitialDatabase>,
    /// What of it a recipe cannot recreate, each with why.
    #[serde(default)]
    pub indexed_db_excluded: Vec<String>,
    /// CacheStorage content within the captured scope coverage, in native cache/key order.
    #[serde(default)]
    pub caches: Vec<crate::cache::InitialCache>,
    #[serde(default)]
    pub cache_excluded: Vec<String>,
}

/// One app state provider's value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppValue {
    pub provider: String,
    pub value: serde_json::Value,
}

/// A live generation's state with its launch inputs and current rules: what a recipe saved from it is
/// built from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeCapture {
    pub live: LiveState,
    pub run: RunId,
    pub profile: DeterminismProfile,
    /// The caller's rules in force, without the launch defaults.
    pub rules: RuleSet,
    pub passthrough_origins: Option<Vec<String>>,
    pub dialog_rules: crate::dialog::DialogRules,
}

impl BasisManifest {
    /// A basis holds exactly the content its kind restores from.
    pub fn validate(&self) -> Result<(), RecipeError> {
        if self.kind == BasisKind::Recipe && !self.verify.is_empty() {
            return Err(invalid("a recipe basis keeps verify in its recipe"));
        }
        validate_verify(&self.verify)?;
        match (self.kind, &self.recipe, &self.replay, &self.checkpoint) {
            (BasisKind::Recipe, Some(_), None, None) | (BasisKind::Replay, None, Some(_), None) => Ok(()),
            // A process image is of one generation at one boundary.
            (BasisKind::Checkpoint, None, None, Some(_)) if self.origin.is_some() => Ok(()),
            (kind, ..) => Err(invalid(format!("a {} basis holds exactly its own content", kind.as_str()))),
        }
    }

    pub fn sha256(&self) -> String {
        crate::canonical::sha256_hex(&crate::canonical::canonical_of(self))
    }

    /// `b-` and the first 16 hex digits of the digest.
    pub fn saved_id(&self) -> SavedId {
        id_of(&self.sha256())
    }
}

pub fn id_of(sha256: &str) -> SavedId {
    SavedId::new(format!("b-{}", &sha256[..16])).expect("hex digits form a managed identifier")
}

/// The basis a child was constructed from, kept in its execution identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchOrigin {
    pub saved_id: SavedId,
    pub sha256: String,
    pub kind: BasisKind,
}

#[cfg(test)]
#[path = "basis_tests.rs"]
mod tests;
