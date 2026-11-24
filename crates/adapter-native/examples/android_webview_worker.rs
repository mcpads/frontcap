//! Private owned worker entry; the controller supplies a fresh directory and stdin authority.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    match arguments.as_slice() {
        [directory] => frontcap_native::android_webview_session::run(std::path::Path::new(directory))?,
        [mode, directory] if mode == "--resource-process" =>
            frontcap_native::resource_process::run(std::path::Path::new(directory))?,
        [mode, directory] if mode == "--resource-group" =>
            frontcap_native::resource_group::run(std::path::Path::new(directory))?,
        _ => return Err("expected session directory, --resource-group directory or --resource-process directory".into()),
    }
    Ok(())
}
