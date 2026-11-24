//! Observed Unicode range editing and the source-owned text receipt.
use super::{ios::ClockIdentity, ios_targets::{Catalog,NotAppliedReason,Role,SelectorKey},
    ios_text::{TextInput,TextRole}, request::{InvalidRequest,canonical_uuid}};
use serde::{Deserialize,Serialize};

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRequest {
    pub generation:String,
    pub request_id:u64,
    pub operation:Operation,
    pub arguments:Arguments,
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Operation { ReplaceText }
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arguments {
    pub basis:TextInput,
    pub start:usize,
    pub end:usize,
    pub text:String,
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Status { Ok }
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Reason { InputUnavailable,InputChanged,CompositionActive,InvalidRange,TextBudget,RangeUnavailable }
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(tag="input",rename_all="snake_case",deny_unknown_fields)]
pub enum SourceResult {
    UikitTextInput { before:TextInput, after:TextInput },
    NotApplied { reason:Reason,current:TextInput },
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub generation:String,
    pub pid:u32,
    pub port:u16,
    pub request:SourceRequest,
    pub last_id:u64,
    pub status:Status,
    pub replayed:bool,
    pub result:SourceResult,
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(tag="input",rename_all="snake_case",deny_unknown_fields)]
pub enum Delivery {
    UikitTextControl { receipt:Box<Receipt> },
    NotApplied { reason:NotAppliedReason },
}
pub fn validate_arguments(start:usize,end:usize,text:&str) -> Result<(),InvalidRequest> {
    if start>end || end>16384 || text.encode_utf16().count()>16384 {
        return Err(InvalidRequest("invalid UIKit edit range or text budget"));
    }
    Ok(())
}
pub fn validate_target<'a>(catalog:&'a Catalog,reference:&str,start:usize,end:usize,text:&str) -> Result<&'a TextInput,InvalidRequest> {
    validate_arguments(start,end,text)?;
    let TextInput::Focused { role,identifier,label,text:original,marked:false,.. }=&catalog.text_input else {
        return Err(InvalidRequest("UIKit edit requires a focused committed-text field"));
    };
    let matches=catalog.targets.iter().any(|t| t.reference==reference && t.unique && t.enabled && t.hittable
        && matches!((t.role,role),(Role::TextField,TextRole::TextField)|(Role::TextView,TextRole::TextView))
        && match t.selector_key { SelectorKey::Identifier => &t.selector_value==identifier, SelectorKey::Label => &t.selector_value==label });
    let units:Vec<u16>=original.encode_utf16().collect();
    let boundary=|n:usize| n<=units.len() && (n==0 || n==units.len()
        || !((0xd800..=0xdbff).contains(&units[n-1]) && (0xdc00..=0xdfff).contains(&units[n])));
    if catalog.transitioning || !matches || !boundary(start) || !boundary(end)
        || units.len()-(end-start)+text.encode_utf16().count()>16384 {
        return Err(InvalidRequest("UIKit edit target or UTF-16 range differs from observation"));
    }
    Ok(&catalog.text_input)
}
impl Delivery {
    pub fn validate(&self,app:&ClockIdentity,start:usize,end:usize,text:&str) -> Result<(),InvalidRequest> {
        validate_arguments(start,end,text)?;
        if let Self::UikitTextControl { receipt:r }=self {
            r.request.arguments.basis.validate()?;
            if !canonical_uuid(&r.generation) || r.generation!=r.request.generation || r.pid!=app.pid
                || r.port==0 || r.last_id==0 || r.last_id>i64::MAX as u64 || r.last_id!=r.request.request_id
                || r.replayed || r.request.arguments.start!=start || r.request.arguments.end!=end
                || r.request.arguments.text!=text || !matches!(r.request.arguments.basis,TextInput::Focused { marked:false,.. }) {
                return Err(InvalidRequest("UIKit source edit receipt differs from request"));
            }
            match &r.result {
                SourceResult::UikitTextInput { before,after } => {
                    before.validate()?;after.validate()?;
                    if before!=&r.request.arguments.basis { return Err(InvalidRequest("UIKit edit basis changed before insertion")); }
                }
                SourceResult::NotApplied { current,.. } => current.validate()?,
            }
        }
        Ok(())
    }
}
#[cfg(test)]
#[path="ios_edit_tests.rs"]
mod tests;
