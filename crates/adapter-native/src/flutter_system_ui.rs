//! Optional system runner configuration for Flutter; request authority is shared with native hosts.
use frontcap_core::native::flutter::Platform;
use std::io;
pub use crate::ios_system_store::{Store, client_directory, read_completion};
pub(crate) use crate::ios_system_store::read_ui_completion;
pub use crate::ios_system_runner::Launch;
pub fn validate_ready(
    config: &crate::flutter_session::Config,
    ready: &crate::flutter_session::Ready,
) -> io::Result<()> {
    match (&config.system_ui, &ready.system_ui) {
        (None, None) => Ok(()),
        (Some(launch), Some(runner)) => {
            launch.validate()?;
            runner.validate().map_err(io::Error::other)?;
            if ready.owner.platform != Platform::Ios
                || !ready.owner.device.eq_ignore_ascii_case(&runner.simulator)
            {
                return Err(io::Error::other(
                    "system runner differs from Flutter device",
                ));
            }
            Ok(())
        }
        _ => Err(io::Error::other(
            "system runner readiness differs from frozen launch",
        )),
    }
}
