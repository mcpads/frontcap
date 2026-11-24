//! `state save`, `describe_saved`, `delete_saved` and `branch`: saved starting points and the independent children
//! constructed from them. These are
//! workspace operations: they name a basis, not a generation.
use crate::connections::from_reply;
use crate::dispatch::{Frontcap, effective_profile, host_class};
use crate::tools::{Common, RecipeArgs, StateOp, ToolError};
use frontcap_core::basis::{BasisKind, BasisManifest, Recipe};
use frontcap_core::ids::{RequestId, SavedId};
use frontcap_core::profile::DeterminismProfile;
use frontcap_core::rules::Action;
use frontcap_recorder::basis::{self, BasisError, SavedBasis};
use frontcap_recorder::branch::{self, BranchError, BranchRequest};
use frontcap_recorder::launcher;
use serde_json::{Value, json};
use std::collections::BTreeMap;

type ToolResult<T> = Result<T, ToolError>;

fn basis_error(e: BasisError) -> ToolError {
    match e {
        BasisError::NotFound(id) => ToolError::new("not_found", format!("no saved basis {id}"), "call state save, then branch the saved_id it returns"),
        BasisError::Corrupt { .. } => ToolError::new("basis_corrupt", e.to_string(), "save the basis again; a changed basis is never used"),
        BasisError::InUse(_) => ToolError::new("busy", e.to_string(), "delete it after that branch or probe returns"),
        BasisError::Io(e) => ToolError::new("internal", e.to_string(), "check the workspace's saved directory"),
    }
}

impl Frontcap {
    pub(crate) async fn saved(&self, c: Common, op: StateOp) -> ToolResult<Value> {
        match op {
            StateOp::Save { kind, recipe, from, verify } => {
                if c.request_id.is_some() {
                    return Err(ToolError::new("invalid_arguments", "state save takes no request_id: the same content always returns the same saved_id", "call state save without request_id"));
                }
                if !verify.is_empty() && recipe.is_some() {
                    return Err(ToolError::new("invalid_arguments", "an authored recipe keeps verify in the recipe",
                        "move the predicates into the recipe's verify"));
                }
                frontcap_core::basis::validate_verify(&verify).map_err(|e| ToolError::new("invalid_arguments", e.to_string(), "fix verify and save again"))?;
                let at_source = match &from {
                    Some(from) => self.verify_at_source(from, &verify).await?,
                    None => Vec::new(),
                };
                let mut saved = match (kind, recipe, from) {
                    (BasisKind::Recipe, Some(recipe), None) => self.save(recipe),
                    (BasisKind::Recipe, None, Some(from)) => self.save_live_recipe(&from, verify).await,
                    (BasisKind::Replay, None, Some(from)) => self.save_replay(&from, verify).await,
                    (BasisKind::Checkpoint, None, Some(from)) => self.save_checkpoint(&from, verify).await,
                    _ => Err(ToolError::new("invalid_arguments", "kind recipe takes a recipe or from (a live generation); kind replay takes from (a live or stopped generation); kind checkpoint takes from (a live generation)", "call state describe")),
                }?;
                if !at_source.is_empty() {
                    saved["verify_at_source"] = json!(at_source);
                }
                Ok(saved)
            }
            StateOp::DescribeSaved { saved_id } => {
                let basis = basis::load(&self.config.workspace, &saved_id).map_err(basis_error)?;
                Ok(summary(&basis, &self.config.workspace))
            }
            StateOp::DeleteSaved { saved_id } => {
                basis::delete(&self.config.workspace, &saved_id).map_err(basis_error)?;
                Ok(json!({"deleted": saved_id}))
            }
            StateOp::Branch { saved_id, kind, changes, progress_ms } => {
                Ok(self.branch(crate::dispatch::request_id(c.request_id)?, saved_id, kind, changes, progress_ms).await?.2)
            }
            StateOp::Probe { saved_id, kind, changes, progress_ms, steps } => self.probe(c.request_id, saved_id, kind, changes, progress_ms, steps).await,
            _ => Err(ToolError::new("internal", "not a saved-basis operation", "report the defect")),
        }
    }

    /// A replay basis: the finalized run of a stopped generation, pinned by its manifest's digest, or
    /// the evidence a live generation has so far, copied at its frozen boundary and kept in the basis.
    /// What each verify predicate reads in a live source now, so a wrong target shows before any branch;
    /// nothing for a stopped one.
    async fn verify_at_source(&self, from: &frontcap_core::ids::LaunchId, verify: &[frontcap_core::basis::Verification]) -> ToolResult<Vec<Value>> {
        let live = frontcap_recorder::runtime::load_runtime(&self.config.workspace.runtime_dir(from)).ok().flatten()
            .is_some_and(|r| r.state == frontcap_recorder::runtime::RuntimeState::Ready);
        let mut read = Vec::new();
        for v in verify.iter().filter(|_| live) {
            let frontcap_recorder_protocol::Observation::Sample { value } = self.observation(from, frontcap_recorder_protocol::ObserveOp::Predicate { predicate: v.predicate.clone() }).await? else {
                return Err(ToolError::new("internal", "unexpected recorder reply", "report the defect"));
            };
            read.push(json!({"name": v.name, "value": value}));
        }
        Ok(read)
    }

    async fn save_replay(&self, from: &frontcap_core::ids::LaunchId, verify: Vec<frontcap_core::basis::Verification>) -> ToolResult<Value> {
        let ws = &self.config.workspace;
        let live = frontcap_recorder::runtime::load_runtime(&ws.runtime_dir(from)).ok().flatten()
            .is_some_and(|r| r.state == frontcap_recorder::runtime::RuntimeState::Ready);
        if live {
            return self.save_live_prefix(from, verify).await;
        }
        let run = launcher::run_id_of(from);
        let dir = self.config.workspace.runs().join(run.as_str());
        let bytes = std::fs::read(dir.join("manifest.json")).map_err(|_| ToolError::new(
            "not_finalized", format!("{from} has no finalized evidence"), "stop the generation first; a replay basis repeats a finalized run's commands"))?;
        let manifest: frontcap_core::bundle::Manifest = serde_json::from_slice(&bytes)
            .map_err(|e| ToolError::new("bundle_corrupt", format!("{run}: {e}"), "record the scenario again"))?;
        if manifest.finalization != frontcap_core::bundle::Finalization::Finalized {
            return Err(ToolError::new("not_finalized", format!("{run} is not finalized"), "stop the generation first"));
        }
        if !manifest.replayable {
            return Err(ToolError::new("not_replayable", format!("{run} is not replayable: missing {:?}", manifest.missing_inputs), "record the scenario again in a run without a recorder fault"));
        }
        let commands = frontcap_core::inputs::read_commands(&dir).map_err(|e| ToolError::new("bundle_corrupt", e.to_string(), "record the scenario again"))?.len();
        let source = frontcap_core::basis::ReplaySource { run, manifest_sha256: frontcap_core::canonical::sha256_bytes_hex(&bytes), commands, kept: false };
        let basis = BasisManifest { kind: BasisKind::Replay, recipe: None, replay: Some(source), checkpoint: None, files: BTreeMap::new(), origin: None, excluded: Vec::new(), verify };
        let saved = basis::save(&self.config.workspace, basis, Vec::new()).map_err(basis_error)?;
        Ok(json!({"saved_id": saved.id, "sha256": saved.sha256, "kind": saved.manifest.kind}))
    }

    /// The live generation's recorder copies its evidence so far into a staging directory at its frozen
    /// boundary; the copy becomes the basis's kept prefix. The generation continues unchanged.
    async fn save_live_prefix(&self, from: &frontcap_core::ids::LaunchId, verify: Vec<frontcap_core::basis::Verification>) -> ToolResult<Value> {
        let ws = &self.config.workspace;
        std::fs::create_dir_all(ws.saved()).map_err(|e| ToolError::new("internal", e.to_string(), "check the workspace"))?;
        let staging = ws.saved().join(format!(".prefix-{}-{}", from.as_str(), RequestId::generate().as_str()));
        struct Removed(std::path::PathBuf);
        impl Drop for Removed {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _removed = Removed(staging.clone());
        let reply = self.connections.call(ws, from, frontcap_recorder_protocol::Call::SavePrefix { dest: staging.clone() }).await?;
        let frontcap_recorder_protocol::Reply::Prefix { run, manifest_sha256, commands } = reply else {
            return Err(ToolError::new("internal", "unexpected recorder reply", "report the defect"));
        };
        let copy = staging.join(run.as_str());
        let manifest: frontcap_core::bundle::Manifest = std::fs::read(copy.join("manifest.json")).ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or_else(|| ToolError::new("internal", "the copied prefix has no manifest", "report the defect"))?;
        if !manifest.replayable {
            return Err(ToolError::new("not_replayable", format!("{from} cannot be re-executed: missing {:?}", manifest.missing_inputs),
                "save a recipe instead, or record the scenario again without a recorder fault"));
        }
        let source = frontcap_core::basis::ReplaySource { run, manifest_sha256, commands, kept: true };
        let basis = BasisManifest { kind: BasisKind::Replay, recipe: None, replay: Some(source), checkpoint: None, files: BTreeMap::new(), origin: None, excluded: Vec::new(), verify };
        let saved = basis::save_kept(ws, basis, &copy).map_err(basis_error)?;
        Ok(json!({"saved_id": saved.id, "sha256": saved.sha256, "kind": saved.manifest.kind}))
    }

    /// A checkpoint basis: the live generation's recorder lets its browser go at the frozen boundary, has
    /// its cell imaged and takes it again; the image, kept under a staging name, becomes the basis. Like a
    /// live replay prefix it needs no lease, and the generation goes on unchanged.
    async fn save_checkpoint(&self, from: &frontcap_core::ids::LaunchId, verify: Vec<frontcap_core::basis::Verification>) -> ToolResult<Value> {
        let ws = &self.config.workspace;
        let staging = format!("s-{}", frontcap_core::ids::LaunchId::generate().as_str().trim_start_matches("g-"));
        let image = || frontcap_recorder::image::image_dir(ws, &staging).ok();
        let reply = match self.connections.call(ws, from, frontcap_recorder_protocol::Call::Image { name: staging.clone() }).await {
            Ok(reply) => reply,
            Err(e) => {
                // The recorder may have kept part of an image before it failed.
                if let Some(dir) = image() {
                    let _ = std::fs::remove_dir_all(dir);
                }
                return Err(e);
            }
        };
        let frontcap_recorder_protocol::Reply::Imaged { boundary } = reply else {
            return Err(ToolError::new("internal", "unexpected recorder reply", "report the defect"));
        };
        let origin = frontcap_core::basis::BasisOrigin { launch_id: from.clone(), run: launcher::run_id_of(from), boundary };
        let excluded = vec![
            "the state of servers the app reached through pass-through: the child's requests reach them as they are then".to_string(),
        ];
        let saved = basis::save_checkpoint(ws, &staging, origin, excluded, verify).map_err(|e| {
            if let Some(dir) = image() {
                let _ = std::fs::remove_dir_all(dir);
            }
            basis_error(e)
        })?;
        Ok(json!({"saved_id": saved.id, "sha256": saved.sha256, "kind": saved.manifest.kind,
            "origin": saved.manifest.origin, "excluded": saved.manifest.excluded}))
    }

    fn save(&self, recipe: Box<RecipeArgs>) -> ToolResult<Value> {
        let r = *recipe;
        let recipe = Recipe {
            url: r.url,
            profile: effective_profile(r.profile),
            rules: r.rules,
            passthrough_origins: r.passthrough_origins,
            barrier: r.barrier,
            changes: r.changes,
            progress_ms: r.progress_ms,
            verify: r.verify,
            dialog_rules: r.dialog_rules,
        };
        self.save_recipe(recipe, None, Vec::new())
    }

    /// A recipe that reconstructs a live generation's state as its frozen boundary has it: the top
    /// document's address, its windows, their local and session storage and the cookies before any
    /// script, then, once the app has settled, the values of its writable state providers. Its launch
    /// profile, rules and passthrough origins are the generation's. What it cannot reconstruct is
    /// listed, not inferred.
    async fn save_live_recipe(&self, from: &frontcap_core::ids::LaunchId, verify: Vec<frontcap_core::basis::Verification>) -> ToolResult<Value> {
        use frontcap_core::intervention::{StateChange, StateField};
        let frontcap_recorder_protocol::Observation::RecipeCapture(capture) = self.observation(from, frontcap_recorder_protocol::ObserveOp::RecipeCapture).await? else {
            return Err(ToolError::new("internal", "unexpected recorder reply", "report the defect"));
        };
        let frontcap_core::basis::RecipeCapture { live, run, mut profile, rules, passthrough_origins, dialog_rules } = *capture;
        // The windows and their storage as they are now replace whatever the launch declared.
        profile.initial_storage = None;
        profile.initial_dom_storage = Some(live.dom_storage);
        profile.initial_contexts = live.contexts;
        profile.initial_window_name = (!live.window_name.is_empty()).then_some(live.window_name);
        let cookie_timing_complete = live.cookies.iter().all(|c| c.timing.is_some());
        profile.initial_cookies = live.cookies;
        if live.storage_inventory_complete {
            // The native partition inventory is authoritative, including absence. Never resurrect
            // deleted buckets, caches or databases from the generation's launch declarations.
            profile.initial_buckets = live.buckets;
            profile.initial_indexed_db = live.indexed_db;
            profile.initial_caches = live.caches;
        } else {
            // Historical captures only covered the top document's default bucket.
            let top = live.url.split_once("://").map(|(scheme, rest)| format!("{scheme}://{}", rest.split(['/', '?', '#']).next().unwrap_or_default()));
            profile.initial_indexed_db.retain(|d| d.bucket.is_some() || d.partition.is_some() || Some(&d.origin) != top.as_ref());
            profile.initial_indexed_db.extend(live.indexed_db);
            profile.initial_caches.retain(|c| c.bucket.is_some() || c.partition.is_some() || Some(&c.origin) != top.as_ref());
            profile.initial_caches.extend(live.caches);
        }
        let changes = live.app.into_iter().map(|a| StateChange {
            target: frontcap_core::predicate::Locator::Document { selector: None }, field: StateField::App { provider: a.provider }, value: a.value, expected: None,
        }).collect();
        let recipe = Recipe { url: live.url, profile, rules, passthrough_origins, barrier: Default::default(), changes, progress_ms: None, verify, dialog_rules };
        let origin = frontcap_core::basis::BasisOrigin { launch_id: from.clone(), run, boundary: live.boundary };
        let mut excluded = [
            "the page's in-memory state (its JavaScript objects, timers and pending work) beyond its app state providers",
            "the document's own state (form values, scroll positions, DOM changes)",
            "the documents of frames and other windows (a closed window is reconstructed closed, at about:blank)",
            "held input, held requests and open dialogs",
        ].map(String::from).to_vec();
        if !live.cookie_inventory_complete {
            excluded.push("cookies outside the top URL (historical capture coverage)".into());
        }
        if !cookie_timing_complete {
            excluded.push("cookie chronology (historical capture coverage)".into());
        }
        if !live.storage_inventory_complete {
            excluded.push("Cache Storage and IndexedDB outside the top document's default bucket (historical capture coverage)".into());
        }
        excluded.extend(live.storage_excluded);
        excluded.extend(live.indexed_db_excluded);
        excluded.extend(live.cache_excluded);
        self.save_recipe(recipe, Some(origin), excluded)
    }

    fn save_recipe(&self, mut recipe: Recipe, origin: Option<frontcap_core::basis::BasisOrigin>, excluded: Vec<String>) -> ToolResult<Value> {
        let kind = BasisKind::Recipe;
        let invalid = |message: String| ToolError::new("invalid_arguments", message, "fix the recipe and save it again");
        recipe.validate().map_err(|e| invalid(e.to_string()))?;
        launcher::validate_inputs(&recipe.profile, recipe.passthrough_origins.as_deref()).map_err(|e| from_reply(e.0))?;
        // A replayed cassette is part of the basis: copy it now, identified by its digest, as launch
        // resolves it against this server's directory.
        let here = std::env::current_dir().map_err(|e| ToolError::new("internal", e.to_string(), "report the defect"))?;
        let mut files = Vec::new();
        let mut digests = BTreeMap::new();
        for rule in recipe.rules.rules.iter_mut() {
            if let Action::Replay { cassette, .. } = &mut rule.action {
                let bytes = std::fs::read(here.join(&*cassette)).map_err(|e| invalid(format!("cassette {cassette} of rule {}: {e}", rule.id)))?;
                let name = basis::file_name(&bytes, "jsonl");
                digests.insert(name.clone(), frontcap_core::canonical::sha256_bytes_hex(&bytes));
                *cassette = name.clone();
                if !files.iter().any(|(n, _): &(String, Vec<u8>)| n == &name) {
                    files.push((name, bytes));
                }
            }
        }
        let manifest = BasisManifest { kind, recipe: Some(recipe), replay: None, checkpoint: None, files: digests, origin, excluded, verify: Vec::new() };
        let saved = basis::save(&self.config.workspace, manifest, files).map_err(basis_error)?;
        if saved.manifest.origin.is_some() {
            return Ok(json!({"saved_id": saved.id, "sha256": saved.sha256, "kind": saved.manifest.kind,
                "origin": saved.manifest.origin, "excluded": saved.manifest.excluded}));
        }
        Ok(json!({"saved_id": saved.id, "sha256": saved.sha256, "kind": saved.manifest.kind}))
    }

    /// The ready child of `saved_id` under `request_id`: its id, its lease (held by this server) and
    /// what `state branch` answers.
    pub(crate) async fn branch(&self, request_id: RequestId, saved_id: SavedId, kind: BasisKind, changes: Vec<frontcap_core::intervention::StateChange>, progress_ms: Option<u64>)
        -> ToolResult<(frontcap_core::ids::LaunchId, frontcap_recorder_protocol::LeaseToken, Value)> {
        let req = BranchRequest {
            request_id,
            saved_id,
            kind,
            changes,
            progress_ms,
            host_class: host_class(),
            watchdog_ms: 10_000,
            repo_root: self.config.frontcap_home.clone(),
        };
        let held = |id: &frontcap_core::ids::LaunchId| self.connections.lease_of(id);
        let built = branch::construct(&self.config.workspace, req, &self.config.recorder_bin, &[], &held).await.map_err(|e| match e {
            BranchError::Basis(e) => basis_error(e),
            BranchError::MethodMismatch(message) => ToolError::new("method_mismatch", message, "branch with the basis's own kind; describe_saved names it"),
            BranchError::Invalid(message) => ToolError::new("invalid_arguments", message, "fix the branch changes and branch again"),
            BranchError::Launch(reply) => from_reply(reply),
            BranchError::Construction { launch_id, phase, detail, bundle } => ToolError::new(
                "construction_failed",
                format!("child {launch_id} failed at {phase}: {detail}; it is stopped with its evidence in {}", bundle.display()),
                "inspect the child's evidence (debug bundle with its launch_id); fix the recipe or the branch changes and branch with a new request_id",
            ),
        })?;
        let status = built.attached.status.clone();
        let launch_id = built.attached.launch_id.clone();
        let lease = built.attached.lease.clone();
        if !built.reused {
            self.connections.hold(launch_id.clone(), lease.clone(), built.attached.client);
        }
        let verification: Vec<Value> = built.verification.iter().map(|(name, value)| json!({"name": name, "value": value})).collect();
        let barrier = built.barrier.map(crate::dispatch::clock_value).transpose()?;
        let child = (launch_id.clone(), lease.clone());
        if built.origin.kind == BasisKind::Checkpoint {
            let mut value = json!({
                "launch_id": launch_id, "lease": lease, "phase": "ready", "origin": built.origin,
                "clock": {"state": "frozen", "t_us": built.boundary.t_us}, "boundary": built.boundary,
                // What construction established before publishing the child.
                "verified": ["the basis's image was unchanged", "the restored browser answered at the image's boundary under this launch"],
                "bundle": status.bundle,
            });
            if !verification.is_empty() {
                value["verification"] = json!(verification);
            }
            // Input the source held at the image, which the child now holds under its lease.
            if status.pointer.is_some() || !status.keys.is_empty() || status.composing {
                value["held"] = json!({"pointer": status.pointer, "keys": status.keys, "composing": status.composing});
            }
            return Ok((child.0, child.1, value));
        }
        if let Some((replayed, commands)) = built.replayed {
            let mut value = json!({
                "launch_id": launch_id, "lease": lease, "phase": "ready", "origin": built.origin,
                "clock": {"state": "frozen", "t_us": built.boundary.t_us}, "boundary": built.boundary,
                "replayed": {"commands": replayed, "of": commands},
                // What construction checked against the recording before publishing the child.
                "verified": ["every command started at its recorded virtual time and answered its recorded outcome",
                    "the behavior log so far equals the beginning of the recorded one (replay projection)"],
                "bundle": status.bundle,
            });
            if !verification.is_empty() {
                value["verification"] = json!(verification);
            }
            return Ok((child.0, child.1, value));
        }
        Ok((child.0, child.1, json!({
            "launch_id": launch_id,
            "lease": lease,
            "phase": "ready",
            "origin": built.origin,
            "clock": {"state": "frozen", "t_us": built.boundary.t_us},
            "boundary": built.boundary,
            "barrier": barrier,
            "changes": built.changes,
            "branch_changes": built.branch_changes,
            "verification": verification,
            "bundle": status.bundle,
        })))
    }
}

/// What a saved basis holds: its structure in full (rule matches, barrier, verification predicates,
/// change targets, the names and keys of initial state), without values that may be private (cookie
/// and storage values, response bodies, change values, cache and record payloads).
fn summary(basis: &SavedBasis, ws: &launcher::Workspace) -> Value {
    if let Some(image) = &basis.manifest.checkpoint {
        return json!({
            "saved_id": basis.id, "sha256": basis.sha256, "kind": basis.manifest.kind,
            "origin": basis.manifest.origin, "excluded": basis.manifest.excluded, "bytes": image.bytes,
            "verify": basis.manifest.verify,
        });
    }
    if let Some(source) = &basis.manifest.replay {
        return json!({
            "saved_id": basis.id, "sha256": basis.sha256, "kind": basis.manifest.kind,
            "origin": {"run": source.run, "manifest_sha256": source.manifest_sha256, "kept": source.kept},
            "commands": source.commands,
            // Where a branch stops: the virtual time the recorded prefix ends at.
            "ends_at_us": basis.replay_source(ws).and_then(|run| std::fs::read_to_string(run.join("trace.jsonl")).ok())
                .and_then(|trace| trace.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()?["t_us"].as_u64()).max()),
            "verify": basis.manifest.verify,
        });
    }
    let Some(r) = &basis.manifest.recipe else { return json!({"saved_id": basis.id, "sha256": basis.sha256, "kind": basis.manifest.kind}) };
    let p: &DeterminismProfile = &r.profile;
    json!({
        "saved_id": basis.id,
        "sha256": basis.sha256,
        "kind": basis.manifest.kind,
        "origin": basis.manifest.origin,
        "excluded": basis.manifest.excluded,
        "verification": "each branch checks verify and reports it",
        "url": r.url,
        "passthrough_origins": r.passthrough_origins,
        "rules": r.rules.rules.iter().map(rule_summary).collect::<Vec<_>>(),
        "initial_state": {
            "window_name": p.initial_window_name,
            "buckets": p.initial_buckets,
            "contexts": p.initial_contexts.iter().enumerate().map(|(index, context)| {
                let mut described = json!({
                    "context": format!("popup-{}", index + 1), "opener": context.opener,
                    "url": context.url, "name": context.name, "no_opener": context.no_opener,
                });
                if context.closed {
                    described["closed"] = json!(true);
                }
                described
            }).collect::<Vec<_>>(),
            "dom_storage": p.initial_dom_storage.as_ref().map(|state| json!({
                "encoding": state.encoding,
                "local": state.local.iter().map(|area| json!({
                    "origin": area.origin, "partition": area.partition, "entries": area.entries.len(),
                    "keys": area.entries.iter().map(|e| local_storage_key(&e.key)).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
                "sessions": state.sessions.iter().map(|session| json!({
                    "context": session.context,
                    "areas": session.areas.iter().map(|area| json!({
                        "origin": area.origin, "partition": area.partition, "entries": area.entries.len(),
                        "keys": area.entries.iter().map(|e| String::from_utf8_lossy(&e.key)).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            })),
            "storage": p.initial_storage.as_ref().map(|s| json!({
                "origin": s.origin, "local_keys": s.local.keys().collect::<Vec<_>>(), "session_keys": s.session.keys().collect::<Vec<_>>(),
            })),
            "cookies": p.initial_cookies.iter().map(|c| json!({
                "origin": c.origin, "name": c.name, "domain": c.domain, "path": c.path, "expires_at_ms": c.expires_at_ms,
                "partition": c.partition, "same_site": c.same_site, "priority": c.priority, "source": c.source, "timing": c.timing,
            })).collect::<Vec<_>>(),
            "caches": p.initial_caches.iter().map(|c| json!({
                "origin": c.origin, "partition": c.partition, "bucket": c.bucket, "name": c.name, "entries": c.entries.len(),
            })).collect::<Vec<_>>(),
            "indexed_db": p.initial_indexed_db.iter().map(|d| json!({
                "origin": d.origin, "partition": d.partition, "bucket": d.bucket, "name": d.name, "version": d.version,
                "stores": d.stores.iter().map(|s| &s.name).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        },
        "dialog_rules": r.dialog_rules,
        "barrier": r.barrier,
        "changes": r.changes.iter().map(|c| json!({"target": c.target, "field": c.field})).collect::<Vec<_>>(),
        "progress_ms": r.progress_ms,
        "verify": r.verify,
        "files": basis.manifest.files.keys().collect::<Vec<_>>(),
    })
}

/// A rule's match and action, with what its answers are (status and latency) but not their bodies or
/// headers.
/// A local storage key as text: Chromium keeps a leading format byte, then Latin-1 or UTF-16LE.
fn local_storage_key(raw: &[u8]) -> String {
    match raw.split_first() {
        Some((1, latin1)) => latin1.iter().map(|&b| char::from(b)).collect(),
        Some((0, utf16)) => char::decode_utf16(utf16.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)))
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect(),
        _ => String::from_utf8_lossy(raw).into_owned(),
    }
}

fn rule_summary(rule: &frontcap_core::rules::Rule) -> Value {
    let answer = |m: &frontcap_core::rules::MockSpec| json!({"status": m.status, "latency_ms": m.latency_ms});
    let mut v = json!({"id": rule.id, "match": rule.matcher, "action": action_name(&rule.action)});
    match &rule.action {
        Action::Mock(m) => v["answers"] = json!([answer(m)]),
        Action::Sequence { steps, after_last } => {
            v["answers"] = steps.iter().map(answer).collect();
            v["after_last"] = json!(after_last);
        }
        Action::Fail { reason } => v["reason"] = json!(reason),
        _ => {}
    }
    v
}

fn action_name(action: &Action) -> &'static str {
    match action {
        Action::Mock(_) => "mock",
        Action::Sequence { .. } => "sequence",
        Action::Hold => "hold",
        Action::Replay { .. } => "replay",
        Action::Record { .. } => "record",
        Action::Passthrough => "passthrough",
        Action::Fail { .. } => "fail",
        Action::StreamMock { .. } => "stream_mock",
    }
}
