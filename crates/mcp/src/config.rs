//! Where the server finds its workspace, the recorder binary, and frontcap's own files.
use frontcap_recorder::launcher::Workspace;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub workspace: Workspace,
    pub recorder_bin: PathBuf,
    /// The directory holding `browsers.toml` and the downloaded browsers.
    pub frontcap_home: PathBuf,
}

impl Config {
    /// `FRONTCAP_WORKSPACE` (default `<cwd>/.frontcap`), `FRONTCAP_RECORDER` (default `frontcap-recorder`
    /// next to this executable), `FRONTCAP_HOME` (default the source tree this binary was built from).
    pub fn from_env() -> std::io::Result<Config> {
        let workspace = match std::env::var_os("FRONTCAP_WORKSPACE") {
            Some(p) => PathBuf::from(p),
            None => std::env::current_dir()?.join(".frontcap"),
        };
        let recorder_bin = match std::env::var_os("FRONTCAP_RECORDER") {
            Some(p) => PathBuf::from(p),
            None => std::env::current_exe()?.with_file_name("frontcap-recorder"),
        };
        let frontcap_home = match std::env::var_os("FRONTCAP_HOME") {
            Some(p) => PathBuf::from(p),
            None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        };
        Ok(Config { workspace: Workspace { root: workspace }, recorder_bin, frontcap_home })
    }
}
