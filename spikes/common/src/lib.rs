pub mod proxy;

use cdp::protocol::{emulation, page, runtime, target};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Browser {
    pub client: cdp::Client,
    pub launched: browser_pin::Launched,
}

pub struct Page {
    pub session: String,
    pub target_id: String,
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

impl Browser {
    pub async fn start(extra_args: &[String]) -> anyhow::Result<Browser> {
        let root = repo_root();
        let pin = browser_pin::load_pin(&root)?;
        let binary = match std::env::var("SPIKE_BROWSER") {
            Ok(path) => PathBuf::from(path),
            Err(_) => browser_pin::install(&root, &pin, browser_pin::current_platform()?)?,
        };
        let launched = browser_pin::launch(&binary, extra_args, Duration::from_secs(30))?;
        let client = cdp::Client::connect(&launched.browser_ws_url).await?;
        Ok(Browser { client, launched })
    }

    pub async fn new_page(&self, begin_frame_control: bool) -> anyhow::Result<Page> {
        let created = self
            .client
            .call(None, &target::CreateTargetParams {
                enable_begin_frame_control: Some(begin_frame_control),
                ..target::CreateTargetParams::new("about:blank".into())
            })
            .await?;
        let attached = self
            .client
            .call(None, &target::AttachToTargetParams {
                flatten: Some(true),
                ..target::AttachToTargetParams::new(created.target_id.clone())
            })
            .await?;
        let page = Page { session: attached.session_id.0, target_id: created.target_id.0 };
        self.client.call(Some(&page.session), &page::EnableParams::default()).await?;
        self.client.call(Some(&page.session), &runtime::EnableParams::default()).await?;
        Ok(page)
    }
}

pub async fn eval(client: &cdp::Client, page: &Page, expr: &str) -> anyhow::Result<serde_json::Value> {
    let r = client
        .call(Some(&page.session), &runtime::EvaluateParams {
            return_by_value: Some(true),
            await_promise: Some(true),
            ..runtime::EvaluateParams::new(expr.into())
        })
        .await?;
    if let Some(ex) = r.exception_details {
        anyhow::bail!("evaluation threw: {}", ex.text);
    }
    Ok(r.result.value.unwrap_or(serde_json::Value::Null))
}

pub async fn set_policy(
    client: &cdp::Client,
    page: &Page,
    policy: emulation::VirtualTimePolicy,
    budget_ms: Option<f64>,
) -> anyhow::Result<f64> {
    let r = client
        .call(Some(&page.session), &emulation::SetVirtualTimePolicyParams {
            budget: budget_ms,
            ..emulation::SetVirtualTimePolicyParams::new(policy)
        })
        .await?;
    Ok(r.virtual_time_ticks_base)
}

pub async fn advance(client: &cdp::Client, page: &Page, ms: f64) -> anyhow::Result<()> {
    let mut expired = client.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>(Some(&page.session));
    set_policy(client, page, emulation::VirtualTimePolicy::Advance, Some(ms)).await?;
    expired
        .next_timeout(Duration::from_secs(30))
        .await?
        .ok_or_else(|| anyhow::anyhow!("virtual time budget of {ms} ms did not expire within 30 s wall"))?;
    Ok(())
}

pub async fn navigate(client: &cdp::Client, page: &Page, url: &str) -> anyhow::Result<()> {
    client.call(Some(&page.session), &page::NavigateParams::new(url.into())).await?;
    Ok(())
}

pub fn serve_dir(dir: &Path) -> anyhow::Result<std::net::SocketAddr> {
    let server = tiny_http::Server::http("127.0.0.1:0").map_err(|e| anyhow::anyhow!("{e}"))?;
    let addr = server.server_addr().to_ip().expect("ip listener");
    let dir = dir.to_path_buf();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let rel = req.url().trim_start_matches('/').split('?').next().unwrap_or("").to_string();
            let path = dir.join(if rel.is_empty() { "index.html".into() } else { rel });
            let resp = match std::fs::read(&path) {
                Ok(bytes) => {
                    let ct = if path.extension().is_some_and(|e| e == "html") { "text/html" } else { "application/json" };
                    tiny_http::Response::from_data(bytes)
                        .with_header(tiny_http::Header::from_bytes("Content-Type", ct).unwrap())
                        .boxed()
                }
                Err(_) => tiny_http::Response::from_string("not found").with_status_code(404).boxed(),
            };
            let _ = req.respond(resp);
        }
    });
    Ok(addr)
}

pub fn report(name: &str, mut value: serde_json::Value) {
    value["spike"] = serde_json::Value::String(name.into());
    value["engine"] = serde_json::Value::String("HeadlessChrome/154.0.8037.92".into());
    value["host"] = serde_json::Value::String(format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH));
    let line = value.to_string();
    println!("{line}");
    let dir = repo_root().join("spikes/results");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.json")), serde_json::to_string_pretty(&value).unwrap()).unwrap();
}
