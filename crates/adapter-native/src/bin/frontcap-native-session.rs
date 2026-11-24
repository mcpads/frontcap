//! Private native device/session owner; not a recorder or MCP entry point.
fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(directory) = args.next().filter(|_| args.len() == 0) else {
        eprintln!("expected an existing native session directory");
        std::process::exit(2);
    };
    if let Err(error) = frontcap_native::session::run(std::path::Path::new(&directory)) {
        eprintln!("native session: {error}");
        std::process::exit(1);
    }
}
