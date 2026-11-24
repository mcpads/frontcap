//! Private iOS worker and resource owners; inherited pipes grant live authority.
fn main() -> std::io::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [directory] => frontcap_native::ios_session::run(std::path::Path::new(directory)),
        [mode, directory] if mode == "--inspect-retirement" => {
            let retirement = frontcap_native::ios_session::inspect(std::path::Path::new(directory))?.retirement();
            println!("{}", serde_json::json!({"retirement": retirement}));
            Ok(())
        }
        [mode, directory] if mode == "--apply-flutter-initial-data" => frontcap_native::flutter_session::apply_initial_data_in_group(std::path::Path::new(directory)),
        [mode, directory] if mode == "--resource-group" => frontcap_native::resource_group::run(std::path::Path::new(directory)),
        [mode, directory] if mode == "--resource-process" => frontcap_native::resource_process::run(std::path::Path::new(directory)),
        _ => Err(std::io::Error::other("expected session directory or private resource entry point")),
    }
}
