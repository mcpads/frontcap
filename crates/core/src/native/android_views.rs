//! Read-only source View snapshots. Tokens are not input references or proof of hit-test readiness.
use super::request::{InvalidRequest, canonical_uuid};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role { View, TextField, Switch, CheckBox, Button, StaticText, ScrollView }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub token: String,
    pub class_name: String,
    pub role: Role,
    pub resource_id: String,
    pub label: String,
    pub text: Option<String>,
    pub error: Option<String>,
    pub scroll: Option<super::android_scroll::Geometry>,
    pub edit_limit_utf16: Option<u32>,
    pub tap_point_screen_px: Option<[u32; 2]>,
    pub secure: bool,
    pub enabled: bool,
    pub visible: bool,
    pub focused: bool,
    pub clickable: bool,
    pub selected: bool,
    pub transformed: bool,
    pub layout_bounds_screen_px: [i64; 4],
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub window_focused: bool,
    pub ime_visible: Option<bool>,
    pub targets: Vec<View>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason { WindowNotLaidOut, TextBudgetExceeded, ViewBudgetExceeded, ResponseBudgetExceeded, CaptureFailed }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Observation {
    Observed { request: String, generation: String, pid: u32, now_ns: u64, catalog: Catalog },
    Unavailable { request: String, generation: String, pid: u32, now_ns: u64, reason: UnavailableReason },
}
impl Observation {
    pub fn validate(&self, expected_request: &str, expected_generation: &str, expected_pid: u32) -> Result<(), InvalidRequest> {
        let (request,generation,pid,now_ns) = match self {
            Self::Observed { request,generation,pid,now_ns,.. } | Self::Unavailable { request,generation,pid,now_ns,.. } => (request,generation,*pid,*now_ns),
        };
        if !canonical_uuid(request) || !canonical_uuid(generation) || pid == 0 || pid > i32::MAX as u32
            || now_ns > i64::MAX as u64 || request != expected_request || generation != expected_generation || pid != expected_pid {
            return Err(InvalidRequest("View observation identity differs from the requested owner"));
        }
        if let Self::Observed { catalog,.. } = self { catalog.validate()?; }
        Ok(())
    }
}
impl Catalog {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        if self.targets.is_empty() || self.targets.len() > 2048 { return Err(InvalidRequest("invalid View catalog size")); }
        let mut tokens = std::collections::BTreeSet::new();
        let mut focused = 0;
        for view in &self.targets {
            let [left,top,right,bottom] = view.layout_bounds_screen_px;
            let dimensions = [right.checked_sub(left),bottom.checked_sub(top)];
            if !canonical_uuid(&view.token) || !tokens.insert(&view.token) || view.class_name.is_empty()
                || [&view.class_name,&view.resource_id,&view.label].iter().any(|s| s.encode_utf16().count() > 4096)
                || view.text.as_ref().is_some_and(|s| s.encode_utf16().count() > 16384)
                || view.edit_limit_utf16.is_some_and(|limit| limit > 16384 || view.role != Role::TextField || view.secure)
                || view.error.as_ref().is_some_and(|s| s.encode_utf16().count() > 4096)
                || (view.secure && (view.text.is_some() || view.error.is_some()))
                || [left,top].iter().any(|n| i32::try_from(*n).is_err())
                || dimensions.iter().any(|n| n.is_none_or(|n| n < 0 || n > i32::MAX as i64 || (view.visible && n == 0))) {
                return Err(InvalidRequest("invalid source View attributes"));
            }
            if let Some([x,y])=view.tap_point_screen_px
                && (!view.visible || !view.enabled || !view.clickable || view.transformed
                    || !matches!(view.role,Role::Button|Role::CheckBox|Role::Switch|Role::TextField)
                    || i64::from(x)<left || i64::from(x)>=right || i64::from(y)<top || i64::from(y)>=bottom) {
                    return Err(InvalidRequest("invalid source tap point"));
                }
            if let Some(scroll)=&view.scroll { scroll.validate(view)?; }
            focused += usize::from(view.focused);
        }
        if focused > 1 { return Err(InvalidRequest("multiple focused source Views")); }
        Ok(())
    }
}
#[cfg(test)]
#[path = "android_views_tests.rs"]
mod tests;
