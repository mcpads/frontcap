//! Browser-derived storage identities without executing an application or granting storage access.
use crate::{ChromiumError, session::Session};
use cdp::protocol::target;
use frontcap_core::storage::StoragePartition;
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

pub struct StorageMetadata {
    session: Session,
    _browser: Helper,
}

/// The helper browser, ended with this value.
enum Helper {
    /// Owned so that dropping it kills the browser and removes its profile.
    Process { _launched: browser_pin::Launched },
    Cell(crate::generation::Cell),
}

impl Drop for Helper {
    fn drop(&mut self) {
        if let Helper::Cell(cell) = self {
            cell.end();
        }
    }
}

#[derive(Serialize)]
struct OriginSite<'a> {
    origin: &'a str,
}
#[derive(Deserialize)]
struct Site {
    site: String,
}
impl cdp::Command for OriginSite<'_> {
    const METHOD: &'static str = "Storage.frontcapGetOriginSite";
    type Returns = Site;
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolveKey<'a> {
    origin: &'a str,
    top_level_site: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    has_cross_site_ancestor: Option<bool>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Key {
    storage_key: String,
}
impl cdp::Command for ResolveKey<'_> {
    const METHOD: &'static str = "Storage.frontcapResolveStorageKey";
    type Returns = Key;
}

impl StorageMetadata {
    /// An owned, blank browser with no application navigation, in `cell` where the host runs browsers in
    /// cells. Drop kills/waits and removes its profile, or ends its cell.
    pub async fn open(repo_root: &Path, cell: Option<crate::generation::Cell>) -> Result<Self, ChromiumError> {
        let (binary, _) =
            browser_pin::engine::patched_binary(repo_root, browser_pin::current_platform()?)?
                .ok_or_else(|| {
                    ChromiumError::Unsupported(
                        "storage relocation requires the current patched engine".into(),
                    )
                })?;
        let args = ["--enable-features=FrontcapFramesWhileFrozen".to_string()];
        let (client, browser) = match cell {
            Some(cell) => {
                let helper = Helper::Cell(cell);
                let Helper::Cell(cell) = &helper else { unreachable!("a cell helper") };
                let started = cell.start(&binary, &args, Duration::from_secs(10))?;
                let url = format!("ws://127.0.0.1:{}{}", started.port, started.path);
                (crate::generation::connect_browser(&url, Some(cell)).await?, helper)
            }
            None => {
                let browser = browser_pin::launch(&binary, &args, Duration::from_secs(10))?;
                (cdp::Client::connect(&browser.browser_ws_url).await?, Helper::Process { _launched: browser })
            }
        };
        let page = client
            .call(None, &target::CreateTargetParams::new("about:blank".into()))
            .await?;
        let attached = client
            .call(
                None,
                &target::AttachToTargetParams {
                    flatten: Some(true),
                    ..target::AttachToTargetParams::new(page.target_id)
                },
            )
            .await?;
        Ok(Self {
            session: Session::new(client, attached.session_id.0),
            _browser: browser,
        })
    }

    pub async fn site(&self, origin: &str) -> Result<String, ChromiumError> {
        Ok(self.session.call(&OriginSite { origin }).await?.site)
    }

    pub async fn key(
        &self,
        origin: &str,
        partition: &StoragePartition,
    ) -> Result<String, ChromiumError> {
        Ok(self
            .session
            .call(&ResolveKey {
                origin,
                top_level_site: &partition.top_level_site,
                has_cross_site_ancestor: partition.has_cross_site_ancestor,
            })
            .await?
            .storage_key)
    }
}
