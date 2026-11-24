//! Supervised shared Activity command delivery; device ownership stays with the caller.
use crate::{
    android_app::Ready,
    process::{Exchange, exchange_typed_recorded},
    request::Completion,
};
use serde::Serialize;
use std::{io, path::Path, process::Command, sync::atomic::AtomicBool, time::Instant};
#[derive(Serialize)]
struct Call<'a, R> {
    owner: &'a Ready,
    request: &'a R,
}
pub(crate) struct Client<'a> {
    pub python: &'a Path,
    pub adb: &'a Path,
    pub owner: &'a Ready,
    pub directory: &'a Path,
}
impl Client<'_> {
    pub(crate) fn exchange<R: Serialize>(
        &self,
        script: &Path,
        evidence: &Path,
        request: &R,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Exchange<Completion>> {
        let Self { python, adb, owner, directory } = self;
        let mut command = Command::new(python);
        command
            .arg(script)
            .arg("--adb")
            .arg(adb)
            .arg("--serial")
            .arg(&owner.serial)
            .arg("--directory")
            .arg(directory)
            .arg("--timeout")
            .arg(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_secs_f64()
                    .clamp(0.001, 300.0)
                    .to_string(),
            );
        exchange_typed_recorded(
            &mut command,
            &Call { owner, request },
            deadline,
            cancelled,
            524288,
            evidence,
        )
    }
}
