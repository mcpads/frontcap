use frontcap_mcp::config::Config;
use frontcap_mcp::dispatch::Frontcap;

const USAGE: &str = "usage: frontcap-mcp                       serve the control MCP on stdio
       frontcap-mcp replay <bundle> [--url <url>]
                                           replay a finalized run; prints the report, exits 0 when equal";

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("frontcap-mcp: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => {}
        ["replay", bundle, rest @ ..] => {
            let url = match rest {
                [] => None,
                ["--url", url] => Some(url.to_string()),
                _ => {
                    eprintln!("{USAGE}");
                    return std::process::ExitCode::from(2);
                }
            };
            return replay(&config, std::path::Path::new(bundle), url).await;
        }
        _ => {
            eprintln!("{USAGE}");
            return std::process::ExitCode::from(2);
        }
    }
    let tools = Frontcap { config, connections: Default::default() };
    match frontcap_mcp::server::serve_stdio(tools).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("frontcap-mcp: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Exit 0 when the replay is equal, 1 when it differs, 2 when it could not run.
async fn replay(config: &Config, bundle: &std::path::Path, url: Option<String>) -> std::process::ExitCode {
    match frontcap_mcp::dispatch::replay(config, bundle, url).await {
        Ok(report) => {
            println!("{}", serde_json::to_string_pretty(&report).expect("serializes"));
            if report.equal { std::process::ExitCode::SUCCESS } else { std::process::ExitCode::FAILURE }
        }
        Err(e) => {
            println!("{}", serde_json::to_string_pretty(&e).unwrap_or_default());
            std::process::ExitCode::from(2)
        }
    }
}
