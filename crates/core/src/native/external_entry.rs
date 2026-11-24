//! OS URL dispatch identity. Acceptance never establishes application delivery or destination.
use super::request::{InvalidRequest, canonical_uuid};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case", deny_unknown_fields)]
pub enum Target {
    Android { serial: String, package: String },
    Ios { simulator: String, bundle: String },
}

impl Target {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        let (device_valid, app) = match self {
            Self::Android { serial, package } => {
                let port = serial.strip_prefix("emulator-").and_then(|s| s.parse::<u16>().ok());
                (port.is_some_and(|p| p > 0 && p.is_multiple_of(2) && *serial == format!("emulator-{p}")), package)
            }
            Self::Ios { simulator, bundle } => (canonical_uuid(simulator), bundle),
        };
        if !device_valid || app.len() > 255 || !app.contains('.')
            || app.split('.').any(|part| part.is_empty() || !part.bytes().all(|c|
                c.is_ascii_alphanumeric() || c == b'_' || (matches!(self, Self::Ios { .. }) && c == b'-')))
        {
            return Err(InvalidRequest("invalid external entry target"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub target: Target,
    pub generation: String,
    pub url: String,
}

impl Request {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        self.target.validate()?;
        if !(canonical_uuid(&self.generation) || super::flutter::valid_generation(&self.generation)) {
            return Err(InvalidRequest("invalid external entry generation"));
        }
        validate_url(&self.url)
    }
}

pub fn validate_url(url: &str) -> Result<(), InvalidRequest> {
    let Some((scheme, rest)) = url.split_once(':') else {
        return Err(InvalidRequest("external entry requires an absolute URI"));
    };
    // Validate transport syntax only. App routes, including malformed/missing note IDs, must
    // still reach the ordinary OS/app routing path rather than a consumer-specific parser.
    if url.len() > 8192 || rest.is_empty()
        || scheme.is_empty() || scheme.len() > 64 || !scheme.as_bytes()[0].is_ascii_alphabetic()
        || !scheme.bytes().all(|c| c.is_ascii_alphanumeric() || b"+.-".contains(&c))
        || url.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(InvalidRequest("invalid external entry generation or URI"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Observation { Unobserved }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Accepted {
    pub request: Request,
    pub confirmation: Observation,
    pub destination: Observation,
}

impl Accepted {
    pub fn new(request: Request) -> Self {
        Self { request, confirmation: Observation::Unobserved, destination: Observation::Unobserved }
    }
}
