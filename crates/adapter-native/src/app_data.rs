//! Build isolated app-data inputs; platform owners may apply them before first launch.
use crate::products::BundleInventory;
use frontcap_core::{
    canonical::{canonical_of, sha256_hex},
    write_budget::{Class, WriteBudget, save_inherited},
};
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

/// Declared container-relative files, bound to one application product.
/// Runtime memory, pending work, random state and external services are not captured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub app_id: String,
    pub product_sha256: String,
    pub inventory: BundleInventory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepared {
    pub child_id: String,
    pub recipe_sha256: String,
    pub recipe: Recipe,
}

impl Recipe {
    pub fn validate(&self) -> io::Result<()> {
        self.inventory.validate()?;
        if self.app_id.is_empty()
            || self.app_id.len() > 255
            || !self
                .app_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || self.product_sha256.len() != 64
            || !self
                .product_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(io::Error::other("invalid app-data product identity"));
        }
        Ok(())
    }

    /// Validate before creating a child. Only `prepared.json` admits a completed construction.
    /// Failed children remain inspectable; they are never overwritten or resumed implicitly.
    pub fn construct(
        &self,
        source: &Path,
        destination: &Path,
        child_id: &str,
        app_id: &str,
        product_sha256: &str,
    ) -> io::Result<Prepared> {
        self.validate()?;
        if self.app_id != app_id
            || self.product_sha256 != product_sha256
            || !frontcap_core::native::request::canonical_uuid(child_id)
        {
            return Err(io::Error::other("app-data construction identity differs"));
        }
        self.inventory.verify(source)?;
        let prepared = Prepared {
            child_id: child_id.into(),
            recipe_sha256: sha256_hex(&canonical_of(self)),
            recipe: self.clone(),
        };
        std::fs::create_dir(destination)?;
        save_inherited(Class::Ordinary, &destination.join("construction.json"), &prepared)?;
        self.inventory
            .copy_verified(source, &destination.join("data"))?;
        save_inherited(Class::Ordinary, &destination.join("prepared.json"), &prepared)?;
        Ok(prepared)
    }
}

impl Prepared {
    /// Read completed construction evidence and revalidate bytes before any platform installation.
    pub fn read(directory: &Path, app_id: &str, product_sha256: &str) -> io::Result<Self> {
        let value: Self = frontcap_core::state::load(&directory.join("prepared.json"))?
            .ok_or_else(|| io::Error::other("app-data child construction is incomplete"))?;
        value.recipe.validate()?;
        if value.recipe.app_id != app_id
            || value.recipe.product_sha256 != product_sha256
            || value.recipe_sha256 != sha256_hex(&canonical_of(&value.recipe))
            || !frontcap_core::native::request::canonical_uuid(&value.child_id)
        {
            return Err(io::Error::other("prepared app-data identity differs"));
        }
        let original: Self = frontcap_core::state::load(&directory.join("construction.json"))?
            .ok_or_else(|| io::Error::other("app-data construction record missing"))?;
        if original != value {
            return Err(io::Error::other("app-data construction record differs"));
        }
        value.recipe.inventory.verify(&directory.join("data"))?;
        Ok(value)
    }
}

/// Only a platform owner that proved a fresh, not-yet-launched installation may call this.
/// Preflight all destinations before writes. Partial failures must prevent app launch.
pub(crate) fn apply_new_files(
    prepared: &Prepared,
    source: &Path,
    container: &Path,
) -> io::Result<()> {
    apply_new_files_with_budget(prepared, source, container, WriteBudget::from_environment()?.as_ref())
}

fn apply_new_files_with_budget(
    prepared: &Prepared,
    source: &Path,
    container: &Path,
    budget: Option<&WriteBudget>,
) -> io::Result<()> {
    use std::fs::{self, File};
    use std::os::unix::fs::PermissionsExt;
    let inventory = &prepared.recipe.inventory;
    inventory.verify(source)?;
    if !fs::symlink_metadata(container)?.is_dir() {
        return Err(io::Error::other(
            "app-data container is not a real directory",
        ));
    }
    for name in &inventory.directories {
        match fs::symlink_metadata(container.join(name)) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::other(
                    "app-data destination contains a link or file",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    for name in inventory.files.keys() {
        match fs::symlink_metadata(container.join(name)) {
            Ok(_) => return Err(io::Error::other("app-data destination file already exists")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    let bytes = inventory.files.values().try_fold(0_u64, |total, file| total.checked_add(file.bytes)
        .ok_or_else(|| io::Error::other("app-data inventory size overflow")))?;
    if let Some(budget) = budget { budget.charge(Class::Ordinary, bytes)?; }
    for name in &inventory.directories {
        let path = container.join(name);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(e)
                if e.kind() == io::ErrorKind::AlreadyExists
                    && fs::symlink_metadata(&path)?.is_dir() => {}
            Err(e) => return Err(e),
        }
    }
    for (name, expected) in &inventory.files {
        let path = container.join(name);
        let mut output = File::create_new(&path)?;
        if &crate::products::read_file_limited(&source.join(name), Some(&mut output), &mut || Ok(()), Some(expected.bytes))? != expected {
            return Err(io::Error::other(
                "prepared app data changed during application",
            ));
        }
        fs::set_permissions(
            &path,
            fs::Permissions::from_mode(if expected.executable { 0o700 } else { 0o600 }),
        )?;
        output.sync_all()?;
        if &crate::products::read_file(&path, None)? != expected {
            return Err(io::Error::other("installed app data differs"));
        }
    }
    for name in inventory.directories.iter().rev() {
        File::open(container.join(name))?.sync_all()?;
    }
    File::open(container)?.sync_all()
}

#[cfg(test)]
#[path = "app_data_tests.rs"]
mod tests;
