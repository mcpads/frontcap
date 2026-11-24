//! Observed Android tap registration and real window DOWN/UP delivery; business effects need observation.
use super::request::{InvalidRequest, canonical_uuid};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub generation: String,
    pub controller_id: String,
    pub request_id: u64,
    pub observation: String,
    pub target: String,
    pub now_ns: u64,
    pub x: u32,
    pub y: u32,
}
impl Request {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        if [&self.generation,&self.controller_id,&self.observation,&self.target].iter().any(|s|!canonical_uuid(s))
            || self.request_id==0 || self.request_id>i64::MAX as u64 || self.now_ns>i64::MAX as u64
            || self.x>i32::MAX as u32 || self.y>i32::MAX as u32 {
            return Err(InvalidRequest("invalid Android tap request"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum State { Armed, Pending, Completed, Rejected }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Reason { StaleObservation, ChangedViewState, UnsupportedTarget, UnexpectedTouch }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub request: Request,
    pub generation: String,
    pub pid: u32,
    pub now_ns: u64,
    pub last_id: u64,
    pub state: State,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub down_handled: Option<bool>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub up_handled: Option<bool>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub reason: Option<Reason>,
}
impl Receipt {
    pub fn validate(&self, request: &Request, generation: &str, pid: u32) -> Result<(), InvalidRequest> {
        request.validate()?;
        if self.request!=*request || self.generation!=generation || request.generation!=generation
            || self.pid!=pid || pid==0 || pid>i32::MAX as u32 || self.last_id!=request.request_id
            || self.now_ns>i64::MAX as u64 || (self.state!=State::Rejected && self.now_ns!=request.now_ns)
            || (self.state==State::Rejected)!=self.reason.is_some()
            || (self.state==State::Completed)!=self.down_handled.is_some()
            || (self.state==State::Completed)!=self.up_handled.is_some() {
            return Err(InvalidRequest("Android tap receipt contradicts bound owner, request or delivery"));
        }
        Ok(())
    }
}
/// Recorder-resolved observation and source-advertised point; callers do not supply coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tap {
    pub observation: crate::ids::RequestId,
    pub source_observation: String,
    pub reference: String,
    pub now_ns: u64,
    pub point: [u32; 2],
}
impl Tap {
    pub fn request(&self, generation: &str, controller: &str, number: u64) -> Result<Request, InvalidRequest> {
        let request=Request {generation:generation.into(),controller_id:controller.into(),request_id:number,
            observation:self.source_observation.clone(),target:self.reference.clone(),now_ns:self.now_ns,
            x:self.point[0],y:self.point[1]};
        request.validate()?;Ok(request)
    }
    pub fn validate_target(&self, capture: &super::android_capture::Capture) -> Result<(), InvalidRequest> {
        capture.validate()?;
        if self.source_observation!=capture.after_request || self.now_ns!=capture.now_ns || !capture.catalog.window_focused {
            return Err(InvalidRequest("Android tap differs from focused source observation"));
        }
        let view=capture.catalog.targets.iter().find(|v|v.token==self.reference)
            .ok_or(InvalidRequest("Android tap target missing"))?;
        if view.tap_point_screen_px!=Some(self.point) || self.point[0]>=capture.size[0] || self.point[1]>=capture.size[1] {
            return Err(InvalidRequest("Android tap requires the observed source point"));
        }
        self.request(&capture.generation,&capture.generation,1).map(|_|())
    }
}
#[cfg(test)]
#[path="android_tap_tests.rs"]
mod tests;
