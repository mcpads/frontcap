//! Shared test-only installed Flutter ownership fixture.
use crate::flutter_session::Ready;
use frontcap_core::native::flutter::{Owner, Platform};
use std::path::Path;

pub(crate) fn acquisition(
    root: &Path,
    generation: &str,
    pid: u32,
) -> (Ready, crate::flutter_ios_app::Acquired) {
    let device = "ABCD0000-0000-0000-0000-000000000001";
    let ready = Ready {
        owner: Owner {
            platform: Platform::Ios,
            device: device.into(),
            generation: generation.into(),
            source_sha256: "f".repeat(64),
        },
        endpoint: crate::flutter_bridge::Endpoint {
            port: 12345,
            token: "d".repeat(48),
        },
        system_ui: None,
        external_url_target: Some(frontcap_core::native::external_entry::Target::Ios {
            simulator: device.to_ascii_lowercase(),
            bundle: "org.example.client".into(),
        }),
    };
    let app = crate::flutter_ios_app::Acquired {
        simulator: device.into(),
        bundle: "org.example.client".into(),
        containers: crate::flutter_ios_app::Containers {
            app: root.join("Client.app"),
            data: root.join("data"),
            data_device: 1,
            data_inode: 2,
            installed_library_modes: Default::default(),
        },
        process: frontcap_core::native::flutter_lifecycle::Process {
            pid,
            birth: format!("process {pid}"),
            command: root.join("Client.app/Runner").display().to_string(),
        },
        endpoint: crate::flutter_ios_app::Endpoint {
            generation: generation.into(),
            token: ready.endpoint.token.clone(),
            port: 12345,
            scenario: "session".into(),
        },
    };
    (ready, app)
}
