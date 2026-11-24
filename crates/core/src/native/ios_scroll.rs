//! Source scroll geometry and one observed XCTest swipe. Offsets are thousandths of a point.
use super::{ios_targets::{Catalog, Role, SelectorKey}, request::{InvalidRequest, canonical_uuid}};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature="schema", derive(schemars::JsonSchema))]
#[serde(rename_all="snake_case")]
pub enum Direction { Up, Down, Left, Right }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scroll {
    pub token:String,
    pub role:Role,
    pub identifier:String,
    pub label:String,
    pub offset_mpt:[i64;2],
    pub minimum_mpt:[i64;2],
    pub maximum_mpt:[i64;2],
    pub viewport_mpt:[i64;2],
    pub enabled:bool,
    pub dragging:bool,
    pub decelerating:bool,
}
impl Scroll {
    pub fn validate(&self)->Result<(),InvalidRequest> {
        if !canonical_uuid(&self.token) || !matches!(self.role,Role::Table|Role::CollectionView|Role::ScrollView|Role::TextView)
            || self.identifier.encode_utf16().count()>4096 || self.label.encode_utf16().count()>4096
            || (self.identifier.is_empty() && self.label.is_empty())
            || self.offset_mpt.iter().chain(&self.minimum_mpt).chain(&self.maximum_mpt).chain(&self.viewport_mpt)
                .any(|n| !(-(i32::MAX as i64)..=i32::MAX as i64).contains(n))
            || (0..2).any(|i| self.minimum_mpt[i]>self.maximum_mpt[i] || self.viewport_mpt[i]<0) {
            return Err(InvalidRequest("invalid UIKit scroll geometry"));
        }
        Ok(())
    }
    pub fn allows(&self,direction:Direction)->bool {
        if !self.enabled || self.dragging || self.decelerating || self.viewport_mpt.contains(&0)
            || (0..2).any(|i| self.offset_mpt[i]<self.minimum_mpt[i] || self.offset_mpt[i]>self.maximum_mpt[i]) { return false; }
        let (axis,forward)=match direction { Direction::Up=>(1,false),Direction::Down=>(1,true),
            Direction::Left=>(0,false),Direction::Right=>(0,true) };
        if forward { self.offset_mpt[axis]<self.maximum_mpt[axis] } else { self.offset_mpt[axis]>self.minimum_mpt[axis] }
    }
}
pub fn target<'a>(catalog:&'a Catalog,reference:&str,direction:Direction)->Result<&'a Scroll,InvalidRequest> {
    let row=catalog.targets.iter().find(|t| t.reference==reference && t.unique && t.enabled && t.hittable)
        .ok_or(InvalidRequest("scroll target is not ready"))?;
    let mut matches=catalog.scrolls.iter().filter(|s| s.role==row.role && match row.selector_key {
        SelectorKey::Identifier=>s.identifier==row.selector_value,SelectorKey::Label=>s.label==row.selector_value });
    let selected=matches.next().ok_or(InvalidRequest("scroll source is unavailable"))?;
    if matches.next().is_some() || catalog.transitioning || !selected.allows(direction) {
        return Err(InvalidRequest("scroll direction is unavailable or source is moving"));
    }
    Ok(selected)
}
pub fn validate_delivery(before:&Scroll,after:&Scroll,direction:Direction)->Result<(),InvalidRequest> {
    before.validate()?;after.validate()?;
    if !before.allows(direction) || before.token!=after.token || before.role!=after.role
        || before.identifier!=after.identifier || before.label!=after.label {
        return Err(InvalidRequest("scroll delivery changed source owner or unavailable direction"));
    }
    Ok(()) // Actual movement can be zero; it does not prove finite end or permission to repeat input.
}
#[cfg(test)]
#[path="ios_scroll_tests.rs"]
mod tests;
