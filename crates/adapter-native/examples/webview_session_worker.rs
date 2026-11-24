//! Private iOS worker and resource owners; inherited pipes grant their live authority.
fn main() -> std::io::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [directory] => frontcap_native::webview_session::run(std::path::Path::new(directory)),
        [mode, directory] if mode == "--resource-group" => frontcap_native::resource_group::run(std::path::Path::new(directory)),
        [mode, directory] if mode == "--resource-process" => frontcap_native::resource_process::run(std::path::Path::new(directory)),
        _ => Err(std::io::Error::other("expected session directory or private resource entry point")),
    }
}
