//! First-launch storage application through owned Android SDK commands, without consumer hooks.
use super::*;
use crate::app_data::Prepared;


impl Device {
    fn initial_data_process_absent(
        &mut self,
        package: &str,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<()> {
        let output = self
            .adb(
                &["shell", "ps", "-A", "-o", "NAME"],
                deadline,
                cancelled,
                false,
            )?
            .unwrap();
        crate::android_app_data::require_process_absent(package, &output)
    }

    pub(super) fn apply_initial_data(
        &mut self,
        package: &str,
        activity: &str,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<()> {
        let source = self
            .config
            .initial_data
            .clone()
            .ok_or_else(|| io::Error::other("initial data absent"))?;
        let digest = self.config.apk_sha256.clone();
        let input = Prepared::read(&source, package, &digest)?;
        let boot = self
            .adb(
                &["shell", "cat", "/proc/sys/kernel/random/boot_id"],
                deadline,
                cancelled,
                false,
            )?
            .unwrap();
        let frozen = self.directory.join("initial-data");
        let child =
            input
                .recipe
                .construct(&source.join("data"), &frozen, boot.trim(), package, &digest)?;
        self.initial_data_process_absent(package, deadline, cancelled)?;
        let before =
            crate::android_installation::installation(package, activity, &digest, |args| {
                Ok(self.adb(args, deadline, cancelled, false)?.unwrap())
            })?;
        let temporary = format!("/data/local/tmp/frontcap-data-{}", child.child_id);
        crate::android_app_data::apply_files(&child, &frozen.join("data"), package, &temporary, |args| {
            Ok(self.adb(args, deadline, cancelled, false)?.unwrap())
        })?;
        self.initial_data_process_absent(package, deadline, cancelled)?;
        let after =
            crate::android_installation::installation(package, activity, &digest, |args| {
                Ok(self.adb(args, deadline, cancelled, false)?.unwrap())
            })?;
        if before != after {
            return Err(io::Error::other("initial data installation changed"));
        }
        save_atomic(
            &self.directory.join("initial-data-applied.json"),
            &(child, after),
        )?;
        self.config.initial_data = None;
        Ok(())
    }
}

#[cfg(test)]
#[path = "device_app_data_tests.rs"]
mod tests;
