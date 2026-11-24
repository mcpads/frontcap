use std::path::PathBuf;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if let [_, mode, directory] = args.as_slice()
        && mode == "--collect-native-diagnostics" {
        return match frontcap_native::diagnostic_log::run(std::path::Path::new(directory)) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(_) => std::process::ExitCode::FAILURE,
        };
    }
    run_recorder(args)
}

#[tokio::main]
async fn run_recorder(args: Vec<String>) -> std::process::ExitCode {
    let (mode, dir) = match args.as_slice() {
        [_, mode, flag, dir] if flag == "--runtime-dir" && (mode == "launch" || mode == "recover") => (mode.clone(), PathBuf::from(dir)),
        _ => {
            eprintln!("usage: frontcap-recorder (launch|recover) --runtime-dir <dir>");
            return std::process::ExitCode::from(2);
        }
    };
    let result = if mode == "launch" { frontcap_recorder::run_launch(&dir).await } else { frontcap_recorder::run_recover(&dir).await };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}
