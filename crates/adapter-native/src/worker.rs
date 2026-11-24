//! Host monotonic deadlines and serialized Android transport choices.
use serde::{Deserialize, Serialize};
use std::io;

/// Input registration route, frozen with the launch. Provider remains an opt-in fixture transport.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputRegistrationTransport {
    #[default]
    Broadcast,
    Provider,
}

/// Clock command route, frozen independently from input registration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockTransport {
    #[default]
    Broadcast,
    Provider,
}

pub(crate) fn monotonic_ns() -> io::Result<u64> {
    // CLOCK_MONOTONIC shares a host-wide origin across the controller and worker. A new worker
    // receives the original deadline, so startup does not grant a fresh operation budget.
    unsafe {
        let mut time: libc::timespec = std::mem::zeroed();
        if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) != 0 {
            return Err(io::Error::last_os_error());
        }
        let seconds = u64::try_from(time.tv_sec).map_err(io::Error::other)?;
        seconds
            .checked_mul(1_000_000_000)
            .and_then(|n| n.checked_add(time.tv_nsec as u64))
            .ok_or_else(|| io::Error::other("monotonic clock exceeds range"))
    }
}
