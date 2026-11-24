//! Android WebView host discovery without a consumer clock or input library.
//! A discovered debugger is not a controlled page or a product Ready receipt.
use serde::{Deserialize, Serialize};
use std::io;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub package: String,
    pub activity: String,
    #[serde(default)]
    pub local_service: Option<LocalService>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalService {
    pub device_port: u16,
    pub host_port: u16,
}
pub(crate) fn has_reverse(inventory: &str, service: &LocalService) -> bool {
    let device = format!("tcp:{}", service.device_port);
    let host = format!("tcp:{}", service.host_port);
    // `adb -s owned-serial reverse --list` is scoped to that guest transport.
    inventory.lines().any(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        fields.len() == 3 && fields[1] == device && fields[2] == host
    })
}
impl Profile {
    pub fn validate(&self) -> io::Result<()> {
        if !frontcap_core::native::android_capture::valid_app(&self.package, &self.activity) {
            return Err(io::Error::other("invalid Android WebView application"));
        }
        if self
            .local_service
            .as_ref()
            .is_some_and(|service| service.device_port == 0 || service.host_port == 0)
        {
            return Err(io::Error::other(
                "local service requires explicit nonzero ports",
            ));
        }
        Ok(())
    }
}
pub use frontcap_core::native::android_webview::Debugger;
/// Android's proc stat comm may contain spaces and parentheses. The start tick is field 22.
pub(crate) fn start_ticks(stat: &str, pid: u32) -> io::Result<u64> {
    if !stat.starts_with(&format!("{pid} (")) {
        return Err(io::Error::other("Android process identity differs"));
    }
    stat.rsplit_once(") ")
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| io::Error::other("invalid Android process start ticks"))
}

/// Match serial, local listener and remote socket together; global ADB inventories include others.
pub(crate) fn has_forward(inventory: &str, debugger: &Debugger, port: u16) -> bool {
    has_forward_endpoint(inventory, &debugger.serial, &debugger.socket, port)
}
pub(crate) fn has_forward_endpoint(inventory: &str, serial: &str, socket: &str, port: u16) -> bool {
    let local = format!("tcp:{port}");
    let remote = format!("localabstract:{socket}");
    inventory.lines().any(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        fields == [serial, local.as_str(), remote.as_str()]
    })
}

/// A debugger page candidate, not a selected or paused application document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub id: String,
    pub url: String,
    pub title: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    pub websocket: String,
}
pub(crate) fn pages(bytes: &[u8], port: u16) -> io::Result<Vec<Page>> {
    let values: Vec<serde_json::Value> = serde_json::from_slice(bytes)?;
    if values.len() > 64 {
        return Err(io::Error::other("too many Android debugger targets"));
    }
    let mut pages = Vec::new();
    for value in values.into_iter().filter(|value| value["type"] == "page") {
        let page: Page = serde_json::from_value(value)?;
        if page.id.is_empty()
            || page.id.len() > 256
            || !page
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            || page.websocket != format!("ws://127.0.0.1:{port}/devtools/page/{}", page.id)
            || pages.iter().any(|other: &Page| other.id == page.id)
        {
            return Err(io::Error::other(
                "Android page endpoint differs from owned forwarding",
            ));
        }
        pages.push(page);
    }
    Ok(pages)
}

#[cfg(test)]
#[path = "android_webview_tests.rs"]
mod tests;
