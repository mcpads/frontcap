//! Relocate only declared storage partition sites; application text is not a site declaration.
use super::ReplayError;
use frontcap_chromium::{ChromiumError, storage_metadata::StorageMetadata};
use frontcap_core::{event::EventKind, profile::DeterminismProfile, storage::StoragePartition};
use std::{collections::BTreeMap, path::Path};

fn error(e: ChromiumError) -> ReplayError {
    ReplayError::Launch(frontcap_recorder_protocol::ErrorReply::new(
        crate::execute::code_of(&e),
        e.to_string(),
    ))
}

pub(super) struct StorageRelocation {
    metadata: StorageMetadata,
    /// The private directory of the helper browser's cell, where the host runs browsers in cells; removed
    /// after the cell ends (fields drop in order).
    _cell_dir: Option<super::Removed>,
    from: String,
    to: String,
    keys: BTreeMap<(String, StoragePartition), String>,
}
impl StorageRelocation {
    pub(super) async fn prepare(
        ws: &crate::launcher::Workspace,
        root: &Path,
        original: &DeterminismProfile,
        moved: &mut DeterminismProfile,
        from_origin: &str,
        to_origin: &str,
    ) -> Result<Option<Self>, ReplayError> {
        if from_origin == to_origin
            || !original
                .initial_caches
                .iter()
                .any(|c| c.partition.is_some())
                && !original
                    .initial_indexed_db
                    .iter()
                    .any(|d| d.partition.is_some())
                && !original
                    .initial_buckets
                    .iter()
                    .any(|b| b.partition.is_some())
        {
            return Ok(None);
        }
        // Every browser on a host that runs browsers in cells runs in one, this blank helper included.
        let refused = |e: String| ReplayError::Io(format!("the storage helper's cell: {e}"));
        let (cell, cell_dir) = match crate::cell_user().map_err(|e| refused(e.to_string()))? {
            Some(user) => {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos());
                let dir = ws
                    .root
                    .join(format!("storage-helper-{}-{nanos}", std::process::id()));
                std::fs::create_dir_all(&dir).map_err(|e| refused(e.to_string()))?;
                let removed = super::Removed(dir.clone());
                crate::private(&dir).map_err(|e| refused(e.to_string()))?;
                let cell = frontcap_chromium::generation::Cell::create(
                    &dir.join("cell"),
                    Path::new(crate::CELL_MOUNT),
                    user,
                )
                .map_err(|e| refused(e.to_string()))?;
                (Some(cell), Some(removed))
            }
            None => (None, None),
        };
        let metadata = StorageMetadata::open(root, cell).await.map_err(error)?;
        let from = metadata.site(from_origin).await.map_err(error)?;
        let to = metadata.site(to_origin).await.map_err(error)?;
        let relocation = Self {
            metadata,
            _cell_dir: cell_dir,
            from,
            to,
            keys: BTreeMap::new(),
        };
        for (source, target) in original
            .initial_buckets
            .iter()
            .zip(&mut moved.initial_buckets)
        {
            relocation
                .partition(
                    source.partition.as_ref(),
                    target.partition.as_mut(),
                    &target.origin,
                )
                .await?;
        }
        for (source, target) in original
            .initial_caches
            .iter()
            .zip(&mut moved.initial_caches)
        {
            relocation
                .partition(
                    source.partition.as_ref(),
                    target.partition.as_mut(),
                    &target.origin,
                )
                .await?;
        }
        for (source, target) in original
            .initial_indexed_db
            .iter()
            .zip(&mut moved.initial_indexed_db)
        {
            relocation
                .partition(
                    source.partition.as_ref(),
                    target.partition.as_mut(),
                    &target.origin,
                )
                .await?;
        }
        Ok(Some(relocation))
    }

    async fn partition(
        &self,
        source: Option<&StoragePartition>,
        target: Option<&mut StoragePartition>,
        origin: &str,
    ) -> Result<bool, ReplayError> {
        if let (Some(source), Some(target)) = (source, target)
            && source.top_level_site == self.from
        {
            target.top_level_site.clone_from(&self.to);
            // A fixed sibling origin becomes cross-site when only the top origin moves.
            // A recorded cross-site ancestor stays cross-site: the intermediate chain may remain.
            if target.has_cross_site_ancestor == Some(false)
                && self.metadata.site(origin).await.map_err(error)? != self.to
            {
                target.has_cross_site_ancestor = Some(true);
            }
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) async fn event(
        &mut self,
        source: &EventKind,
        target: &mut EventKind,
    ) -> Result<(), ReplayError> {
        let (source_partition, origin, partition, storage_key) = match (source, target) {
            (EventKind::CacheInitialized(source), EventKind::CacheInitialized(target)) => (
                source.cache.partition.as_ref(),
                &target.cache.origin,
                target.cache.partition.as_mut(),
                &mut target.storage_key,
            ),
            (EventKind::IndexedDbInitialized(source), EventKind::IndexedDbInitialized(target)) => (
                source.database.partition.as_ref(),
                &target.database.origin,
                target.database.partition.as_mut(),
                &mut target.storage_key,
            ),
            _ => return Ok(()),
        };
        let Some(partition) = partition else {
            return Ok(());
        };
        if !self
            .partition(source_partition, Some(partition), origin)
            .await?
        {
            return Ok(());
        }
        // StorageKey is opaque. Ask the same native implementation that seeds the target,
        // instead of replacing strings inside a serialized key or unrelated origin.
        let identity = (origin.clone(), partition.clone());
        let key = match self.keys.get(&identity) {
            Some(key) => key.clone(),
            None => {
                let key = self.metadata.key(origin, partition).await.map_err(error)?;
                self.keys.insert(identity, key.clone());
                key
            }
        };
        *storage_key = key;
        Ok(())
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod bucket_policy_relocation;
