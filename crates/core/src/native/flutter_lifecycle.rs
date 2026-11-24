//! Same-install replacement evidence. These records grant no process attachment authority.
use super::{
    flutter::{Owner, Platform},
    request::InvalidRequest,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Cold URL replacement is currently available only for an owned standalone Android product.
/// iOS needs a separate confirmation phase while its replacement process is still absent.
pub fn restart_entry(owner: &Owner, target: Option<&super::external_entry::Target>, url: Option<&str>)
    -> Result<Option<super::external_entry::Request>, InvalidRequest> {
    let Some(url) = url else { return Ok(None); };
    if owner.platform != Platform::Android {
        return Err(InvalidRequest("cold Flutter URL replacement is unavailable on this host"));
    }
    let target = target.ok_or(InvalidRequest("cold Flutter entry needs an installed application"))?;
    owner.validate_external_target(target)?;
    let request = super::external_entry::Request {
        target: target.clone(), generation: owner.generation.clone(), url: url.into(),
    };
    request.validate()?;
    Ok(Some(request))
}

/// iOS cold delivery requires separate begin/readiness phases, not a blocking restart.
pub fn ios_entry(owner: &Owner, target: Option<&super::external_entry::Target>, url: &str)
    -> Result<super::external_entry::Request, InvalidRequest> {
    owner.validate()?;
    if owner.platform != Platform::Ios {
        return Err(InvalidRequest("two-phase Flutter entry requires iOS"));
    }
    let target = target.ok_or(InvalidRequest("cold iOS entry needs an installed application"))?;
    owner.validate_external_target(target)?;
    let request = super::external_entry::Request {
        target: target.clone(), generation: owner.generation.clone(), url: url.into(),
    };
    request.validate()?;
    Ok(request)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub pid: u32,
    /// SDK-worker-local `ps` birth/command evidence, never a cross-host signalling identity.
    pub birth: String,
    pub command: String,
}
impl Process {
    /// Compare the executable inside the installed bundle, excluding launch arguments.
    /// The worker separately verifies it against CFBundleExecutable before publishing evidence.
    pub fn app_executable<'a>(&'a self, app: &Path) -> Result<&'a str, InvalidRequest> {
        let relative = app
            .to_str()
            .filter(|_| app.is_absolute())
            .and_then(|app| self.command.strip_prefix(app))
            .and_then(|suffix| suffix.strip_prefix('/'))
            .ok_or(InvalidRequest("Flutter process is outside its installed app"))?;
        let executable = relative.split(' ').next().unwrap_or_default();
        if executable.is_empty() || executable == "." || executable == ".." || executable.contains('/') {
            return Err(InvalidRequest("invalid installed Flutter executable"));
        }
        Ok(executable)
    }
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        if self.pid == 0
            || self.pid > i32::MAX as u32
            || self.birth.is_empty()
            || self.birth.len() > 128
            || self.command.is_empty()
            || self.command.len() > 16384
            || self
                .birth
                .chars()
                .chain(self.command.chars())
                .any(char::is_control)
        {
            return Err(InvalidRequest("invalid Flutter process evidence"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installation {
    pub bundle: String,
    pub app: PathBuf,
    pub data: PathBuf,
    pub data_device: u64,
    pub data_inode: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IosReplacement {
    pub previous: Owner,
    pub owner: Owner,
    pub previous_process: Process,
    pub process: Process,
    pub installation: Installation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<super::external_entry::Accepted>,
}
impl IosReplacement {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        self.previous.validate()?;
        self.owner.validate()?;
        self.previous_process.validate()?;
        self.process.validate()?;
        if let Some(entry) = &self.entry {
            let target = super::external_entry::Target::Ios {
                simulator: self.previous.device.to_ascii_lowercase(), bundle: self.installation.bundle.clone(),
            };
            if ios_entry(&self.previous, Some(&target), &entry.request.url)? != entry.request {
                return Err(InvalidRequest("cold iOS entry differs from replacement owner"));
            }
        }
        self.owner
            .validate_external_target(&super::external_entry::Target::Ios {
                simulator: self.owner.device.to_ascii_lowercase(),
                bundle: self.installation.bundle.clone(),
            })?;
        if self.owner.platform != Platform::Ios
            || self.owner.platform != self.previous.platform
            || self.owner.device != self.previous.device
            || self.owner.source_sha256 != self.previous.source_sha256
            || self.owner.generation == self.previous.generation
            || (self.process.pid == self.previous_process.pid
                && self.process.birth == self.previous_process.birth)
            || self.process.app_executable(&self.installation.app)?
                != self.previous_process.app_executable(&self.installation.app)?
            || !self.installation.app.is_absolute()
            || !self.installation.data.is_absolute()
            || self.installation.app == self.installation.data
        {
            return Err(InvalidRequest("Flutter replacement lineage differs"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AndroidReplacement {
    pub previous: Owner,
    pub owner: Owner,
    pub previous_process: super::android_lifecycle::Process,
    pub process: super::android_lifecycle::Process,
    pub installation: super::android_lifecycle::Installation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<super::external_entry::Accepted>,
}
impl AndroidReplacement {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        self.previous.validate()?;
        self.owner
            .validate_external_target(&super::external_entry::Target::Android {
                serial: self.owner.device.clone(),
                package: self.installation.package.clone(),
            })?;
        self.previous_process.validate(&self.installation.package)?;
        self.process.validate(&self.installation.package)?;
        self.installation
            .validate(&self.installation.package, &self.installation.apk_sha256)?;
        if let Some(entry) = &self.entry {
            let target = super::external_entry::Target::Android {
                serial: self.previous.device.clone(), package: self.installation.package.clone(),
            };
            if restart_entry(&self.previous, Some(&target), Some(&entry.request.url))?.as_ref() != Some(&entry.request) {
                return Err(InvalidRequest("cold Flutter entry differs from replacement owner"));
            }
        }
        if self.owner.platform != Platform::Android
            || self.previous.platform != self.owner.platform
            || self.previous.device != self.owner.device
            || self.previous.source_sha256 != self.owner.source_sha256
            || self.previous.generation == self.owner.generation
            || self.previous_process.boot_id != self.process.boot_id
            || self.previous_process == self.process
        {
            return Err(InvalidRequest(
                "Flutter Android replacement lineage differs",
            ));
        }
        Ok(())
    }
}

/// Keep each platform's process and installation evidence together; existing iOS records retain
/// their wire representation. Android process birth is a guest boot/tick identity, not host `ps`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Replacement {
    Ios(IosReplacement),
    Android(AndroidReplacement),
}
impl Replacement {
    pub fn previous(&self) -> &Owner {
        match self {
            Self::Ios(r) => &r.previous,
            Self::Android(r) => &r.previous,
        }
    }
    pub fn owner(&self) -> &Owner {
        match self {
            Self::Ios(r) => &r.owner,
            Self::Android(r) => &r.owner,
        }
    }
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        match self {
            Self::Ios(r) => r.validate(),
            Self::Android(r) => r.validate(),
        }
    }
}

/// Same-process host visibility. It establishes neither app clock advancement nor page lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case", deny_unknown_fields)]
pub enum Transition {
    Ios {
        owner: Owner,
        requested: super::app_state::State,
        process_before: Process,
        process_after: Process,
        installation: Installation,
        runner: super::ios_ui::UiIdentity,
        completion: Box<super::ios_ui::UiCompletion>,
    },
    Android {
        owner: Owner,
        requested: super::app_state::State,
        process_before: super::android_lifecycle::Process,
        process_after: super::android_lifecycle::Process,
        installation: super::android_lifecycle::Installation,
        activity: String,
        focused_before: String,
        focused_after: String,
    },
}
impl Transition {
    pub fn owner(&self) -> &Owner { match self { Self::Ios {owner,..} | Self::Android {owner,..} => owner } }
    pub fn requested(&self) -> super::app_state::State { match self { Self::Ios {requested,..} | Self::Android {requested,..} => *requested } }
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        self.owner().validate()?;
        match self {
            Self::Ios {owner,requested,process_before,process_after,installation,runner,completion} => {
                process_before.validate()?; process_after.validate()?;
                owner.validate_external_target(&super::external_entry::Target::Ios {
                    simulator: runner.simulator.to_ascii_lowercase(), bundle: installation.bundle.clone() })?;
                process_before.app_executable(&installation.app)?;
                completion.validate_system(runner,&completion.request)?;
                let expected = super::ios_ui::UiOperation::SetHostAppState {
                    bundle:installation.bundle.clone(),pid:process_before.pid,state:*requested };
                if owner.platform != Platform::Ios || process_before != process_after || completion.request.operation != expected
                    || !installation.data.is_absolute() || installation.data == installation.app || installation.data_inode == 0 {
                    return Err(InvalidRequest("Flutter iOS host transition changed its live app"));
                }
            },
            Self::Android {owner,requested,process_before,process_after,installation,activity,focused_before,focused_after} => {
                owner.validate_external_target(&super::external_entry::Target::Android {
                    serial:owner.device.clone(),package:installation.package.clone() })?;
                installation.validate(&installation.package,&installation.apk_sha256)?;
                process_before.validate(&installation.package)?; process_after.validate(&installation.package)?;
                let component=format!("{}/{}",installation.package,activity);
                let valid_focus=|value:&str| value.split_once('/').is_some_and(|(package,activity)|
                    super::android_capture::valid_app(package,activity));
                let changed=match requested {
                    super::app_state::State::Background => focused_before == &component && focused_after.split('/').next() != Some(installation.package.as_str()),
                    super::app_state::State::Foreground => focused_before.split('/').next() != Some(installation.package.as_str()) && focused_after == &component,
                };
                if owner.platform != Platform::Android || process_before != process_after || !changed
                    || !super::android_capture::valid_app(&installation.package,activity) || !valid_focus(focused_before) || !valid_focus(focused_after) {
                    return Err(InvalidRequest("Flutter Android host transition changed process or focus"));
                }
            },
        }
        Ok(())
    }
}
