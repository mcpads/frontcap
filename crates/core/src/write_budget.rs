//! Durable, conservative byte admission shared by cooperating writers.
//!
//! Charge before producing bytes. Charges are never refunded, even if the writer dies or its
//! write fails. This bounds admitted payload, not filesystem allocation or uncooperative writers.
use crate::{ids::LaunchId, state};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub ordinary: u64,
    pub retirement: u64,
}

#[derive(Clone, Copy, Debug)]
pub enum Class {
    Ordinary,
    Retirement,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    owner: LaunchId,
    limits: Limits,
    charged: Limits,
}

/// A budget directory is immutable launch input, not cleanup authority. Only cooperating writers
/// participate; callers must separately constrain device/SDK writers and account for metadata.
/// Handles reopen the lock independently so threads and processes serialize alike.
#[derive(Clone)]
pub struct WriteBudget {
    directory: PathBuf,
    owner: LaunchId,
    limits: Limits,
    directory_identity: (u64, u64),
    lock_identity: (u64, u64),
}

fn identity(path: &Path, directory: bool) -> io::Result<(u64, u64)> {
    let metadata = fs::symlink_metadata(path)?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err(io::Error::other("write budget path has wrong type"));
    }
    Ok((metadata.dev(), metadata.ino()))
}

const CHILD_ENVIRONMENT: &str = "FRONTCAP_NATIVE_WRITE_BUDGET";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    directory: PathBuf,
    owner: LaunchId,
    limits: Limits,
    directory_identity: (u64, u64),
    lock_identity: (u64, u64),
}

/// Publish a worker-owned record through its inherited allowance, preserving unbound standalone use.
/// Callers must explicitly classify ordinary work versus owner retirement. This does not admit
/// directories or external writers, and publication failure never refunds an admitted payload.
pub fn save_inherited<T: Serialize>(class: Class, path: &Path, value: &T) -> io::Result<()> {
    match WriteBudget::from_environment()? {
        Some(budget) => budget.save_atomic(class, path, value),
        None => state::save_atomic(path, value),
    }
}

impl WriteBudget {
    /// Persist a binding identity without granting authority to recreate or refill an allowance.
    pub fn binding_sha256(&self) -> io::Result<String> {
        self.charged()?;
        Ok(crate::canonical::sha256_hex(&crate::canonical::canonical_of(&Binding {
            directory: self.directory.clone(), owner: self.owner.clone(), limits: self.limits,
            directory_identity: self.directory_identity, lock_identity: self.lock_identity,
        })))
    }

    /// Pass an existing allowance to a cooperating child without changing this process's environment.
    /// Omission explicitly removes any inherited binding from this launch boundary.
    pub fn configure_child(
        command: &mut std::process::Command,
        budget: Option<&Self>,
    ) -> io::Result<()> {
        if let Some(budget) = budget {
            budget.charged()?;
            if !budget.directory.is_absolute() {
                return Err(io::Error::other(
                    "child write budget requires an absolute directory",
                ));
            }
            let binding = serde_json::to_string(&Binding {
                directory: budget.directory.clone(), owner: budget.owner.clone(), limits: budget.limits,
                directory_identity: budget.directory_identity, lock_identity: budget.lock_identity,
            })?;
            if binding.len() > 4096 {
                return Err(io::Error::other("child write budget binding exceeds limit"));
            }
            command.env(CHILD_ENVIRONMENT, binding);
        } else {
            command.env_remove(CHILD_ENVIRONMENT);
        }
        Ok(())
    }

    /// Open a parent-provided allowance. Invalid or replaced bindings never fall back to unlimited writes.
    pub fn from_environment() -> io::Result<Option<Self>> {
        let value = match std::env::var(CHILD_ENVIRONMENT) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(error) => return Err(io::Error::other(error)),
        };
        if value.len() > 4096 {
            return Err(io::Error::other("child write budget binding exceeds limit"));
        }
        let binding: Binding = serde_json::from_str(&value)?;
        if !binding.directory.is_absolute() {
            return Err(io::Error::other(
                "child write budget directory is not absolute",
            ));
        }
        let budget = Self::open(&binding.directory, binding.owner, binding.limits)?;
        if budget.directory_identity != binding.directory_identity
            || budget.lock_identity != binding.lock_identity
        {
            return Err(io::Error::other(
                "child write budget binding identity changed",
            ));
        }
        Ok(Some(budget))
    }

    /// Exclusively create once, before starting writers. Partial creation is never adopted.
    pub fn create(directory: &Path, owner: LaunchId, limits: Limits) -> io::Result<Self> {
        if limits.ordinary == 0 || limits.retirement == 0 {
            return Err(io::Error::other("write allowances must both be positive"));
        }
        fs::create_dir(directory)?;
        let lock = File::options()
            .write(true)
            .create_new(true)
            .open(directory.join("lock"))?;
        lock.sync_all()?;
        state::save_atomic(
            &directory.join("ledger.json"),
            &Ledger {
                owner: owner.clone(),
                limits,
                charged: Limits {
                    ordinary: 0,
                    retirement: 0,
                },
            },
        )?;
        // Persist the exclusive directory entry as well as its initial contents.
        File::open(directory.parent().unwrap_or(Path::new(".")))?.sync_all()?;
        Self::open(directory, owner, limits)
    }

    /// Reopen only a complete matching budget. Missing/corrupt state never grants a new allowance.
    pub fn open(directory: &Path, owner: LaunchId, limits: Limits) -> io::Result<Self> {
        let budget = Self {
            directory: directory.to_owned(),
            owner,
            limits,
            directory_identity: identity(directory, true)?,
            lock_identity: identity(&directory.join("lock"), false)?,
        };
        budget.charged()?;
        Ok(budget)
    }

    fn locked(&self) -> io::Result<(File, Ledger)> {
        self.check_identity()?;
        let lock = File::options()
            .read(true)
            .write(true)
            .open(self.directory.join("lock"))?;
        lock.lock()?;
        let metadata = lock.metadata()?;
        if (metadata.dev(), metadata.ino()) != self.lock_identity {
            return Err(io::Error::other("write budget lock changed"));
        }
        self.check_identity()?;
        let metadata = fs::symlink_metadata(self.directory.join("ledger.json"))?;
        if !metadata.is_file() || metadata.len() > 4096 {
            return Err(io::Error::other(
                "write budget ledger has wrong type or size",
            ));
        }
        let ledger: Ledger = state::load(&self.directory.join("ledger.json"))?
            .ok_or_else(|| io::Error::other("write budget ledger missing"))?;
        if ledger.owner != self.owner
            || ledger.limits != self.limits
            || ledger.limits.ordinary == 0
            || ledger.limits.retirement == 0
            || ledger.charged.ordinary > ledger.limits.ordinary
            || ledger.charged.retirement > ledger.limits.retirement
        {
            return Err(io::Error::other(
                "write budget identity, limits or charges differ",
            ));
        }
        Ok((lock, ledger))
    }

    fn check_identity(&self) -> io::Result<()> {
        if identity(&self.directory, true)? != self.directory_identity
            || identity(&self.directory.join("lock"), false)? != self.lock_identity
        {
            return Err(io::Error::other("write budget directory or lock changed"));
        }
        Ok(())
    }

    /// Reserve before any external effect or payload publication. Success is durable; errors must
    /// refuse dispatch. A failed synchronization may have charged bytes, never authorize a retry
    /// without another charge. No destination write or deletion is performed here.
    pub fn charge(&self, class: Class, bytes: u64) -> io::Result<()> {
        let (_lock, mut ledger) = self.locked()?;
        let (charged, maximum) = match class {
            Class::Ordinary => (&mut ledger.charged.ordinary, ledger.limits.ordinary),
            Class::Retirement => (&mut ledger.charged.retirement, ledger.limits.retirement),
        };
        let next = charged
            .checked_add(bytes)
            .filter(|next| *next <= maximum)
            .ok_or_else(|| io::Error::other("write allowance exhausted"))?;
        if next != *charged {
            *charged = next;
            self.check_identity()?;
            state::save_atomic(&self.directory.join("ledger.json"), &ledger)?;
        }
        Ok(())
    }

    /// Charge the complete serialized replacement before creating its atomic-write temporary.
    /// Failed writes retain their charge; overwritten or removed files do not refund it.
    pub fn save_atomic<T: Serialize>(
        &self,
        class: Class,
        path: &Path,
        value: &T,
    ) -> io::Result<()> {
        let bytes = crate::canonical::canonical_of(value);
        self.charge(class, bytes.len() as u64)?;
        state::save_atomic_bytes(path, bytes.as_bytes())
    }

    pub fn save_atomic_bytes(&self, class: Class, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.charge(class, bytes.len() as u64)?;
        state::save_atomic_bytes(path, bytes)
    }

    pub fn charged(&self) -> io::Result<Limits> {
        let (_lock, ledger) = self.locked()?;
        Ok(ledger.charged)
    }
}

#[cfg(test)]
#[path = "write_budget_tests.rs"]
mod tests;
