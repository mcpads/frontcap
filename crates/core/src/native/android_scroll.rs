//! Source-observed stock Android View scroll geometry and window gesture receipts.
use super::request::{InvalidRequest, canonical_uuid};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Direction { Up, Down, Left, Right }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gesture { pub direction: Direction, pub start_px: [u32;2], pub end_px: [u32;2] }
impl Gesture {
    pub fn validate(&self) -> Result<(),InvalidRequest> {
        let [x,y]=self.start_px;let [ex,ey]=self.end_px;
        if [x,y,ex,ey].iter().any(|v|*v>i32::MAX as u32) || !match self.direction {
            Direction::Down=>x==ex && y>ey,Direction::Up=>x==ex && y<ey,
            Direction::Right=>y==ey && x>ex,Direction::Left=>y==ey && x<ex,
        } {return Err(InvalidRequest("invalid Android scroll trajectory"));}
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Geometry {
    pub offset_px: [i32;2],
    pub maximum_px: [u32;2],
    pub viewport_px: [u32;2],
    pub gestures: Vec<Gesture>,
}
impl Geometry {
    pub fn validate(&self,view:&super::android_views::View) -> Result<(),InvalidRequest> {
        use super::android_views::Role;
        let vertical=view.class_name=="android.widget.ScrollView";
        if (!vertical && view.class_name!="android.widget.HorizontalScrollView") || view.role!=Role::ScrollView
            || self.maximum_px.iter().chain(&self.viewport_px).any(|v|*v>i32::MAX as u32)
            || self.maximum_px[if vertical {0}else{1}]!=0 || self.gestures.len()>2 {
            return Err(InvalidRequest("unsupported Android scroll geometry"));
        }
        let mut directions=Vec::new();
        for gesture in &self.gestures {
            gesture.validate()?;
            let axis=if vertical {1}else{0};
            let offset=self.offset_px[axis];let maximum=i64::from(self.maximum_px[axis]);
            if !view.visible || !view.enabled || view.transformed || self.viewport_px.contains(&0)
                || self.offset_px[1-axis]!=0 || offset<0 || i64::from(offset)>maximum
                || directions.contains(&gesture.direction) || !match gesture.direction {
                    Direction::Up=>vertical && offset>0,Direction::Down=>vertical && i64::from(offset)<maximum,
                    Direction::Left=>!vertical && offset>0,Direction::Right=>!vertical && i64::from(offset)<maximum,
                } {return Err(InvalidRequest("invalid Android scroll availability"));}
            for [x,y] in [gesture.start_px,gesture.end_px] {
                let [left,top,right,bottom]=view.layout_bounds_screen_px;
                if i64::from(x)<left || i64::from(x)>=right || i64::from(y)<top || i64::from(y)>=bottom {
                    return Err(InvalidRequest("Android scroll trajectory leaves source bounds"));
                }
            }
            directions.push(gesture.direction);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub generation:String,pub controller_id:String,pub request_id:u64,pub observation:String,pub target:String,
    pub now_ns:u64,pub x:u32,pub y:u32,pub end_x:u32,pub end_y:u32,pub direction:Direction,
}
impl Request {
    pub fn validate(&self)->Result<(),InvalidRequest> {
        if [&self.generation,&self.controller_id,&self.observation,&self.target].iter().any(|v|!canonical_uuid(v))
            || self.request_id==0 || self.request_id>i64::MAX as u64 || self.now_ns>i64::MAX as u64 {
            return Err(InvalidRequest("invalid Android scroll identity"));
        }
        Gesture {direction:self.direction,start_px:[self.x,self.y],end_px:[self.end_x,self.end_y]}.validate()
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub request:Request,pub generation:String,pub pid:u32,pub now_ns:u64,pub last_id:u64,
    pub state:super::android_tap::State,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub reason:Option<super::android_tap::Reason>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub down_handled:Option<bool>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub up_handled:Option<bool>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub moves:Option<u32>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub before_offset_px:Option<[i32;2]>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub after_offset_px:Option<[i32;2]>,
}
impl Receipt {
    pub fn validate(&self,request:&Request,generation:&str,pid:u32)->Result<(),InvalidRequest> {
        use super::android_tap::State;
        request.validate()?;
        let done=self.state==State::Completed;
        if self.request!=*request || self.generation!=generation || request.generation!=generation
            || self.pid!=pid || pid==0 || pid>i32::MAX as u32 || self.last_id!=request.request_id || self.now_ns>i64::MAX as u64
            || (self.state!=State::Rejected && self.now_ns!=request.now_ns) || (self.state==State::Rejected)!=self.reason.is_some()
            || done!=self.down_handled.is_some() || done!=self.up_handled.is_some() || done!=self.moves.is_some()
            || done!=self.before_offset_px.is_some() || done!=self.after_offset_px.is_some() || self.moves==Some(0) {
            return Err(InvalidRequest("Android scroll receipt contradicts owner, request or delivery"));
        }
        Ok(())
    }
}
/// Recorder-owned capture basis; transport coordinates come only from the saved source trajectory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scroll {
    pub observation:crate::ids::RequestId,
    pub source_observation:String,
    pub reference:String,
    pub now_ns:u64,
    pub gesture:Gesture,
    pub before_offset_px:[i32;2],
}
impl Scroll {
    pub fn request(&self,generation:&str,controller:&str,number:u64)->Result<Request,InvalidRequest> {
        let request=Request {generation:generation.into(),controller_id:controller.into(),request_id:number,
            observation:self.source_observation.clone(),target:self.reference.clone(),now_ns:self.now_ns,
            x:self.gesture.start_px[0],y:self.gesture.start_px[1],end_x:self.gesture.end_px[0],end_y:self.gesture.end_px[1],
            direction:self.gesture.direction};
        request.validate()?;Ok(request)
    }
    pub fn validate_target(&self,capture:&super::android_capture::Capture)->Result<(),InvalidRequest> {
        capture.validate()?;
        if self.source_observation!=capture.after_request || self.now_ns!=capture.now_ns || !capture.catalog.window_focused {
            return Err(InvalidRequest("Android scroll differs from focused source observation"));
        }
        let geometry=capture.catalog.targets.iter().find(|v|v.token==self.reference).and_then(|v|v.scroll.as_ref())
            .ok_or(InvalidRequest("Android scroll target missing or unsupported"))?;
        if geometry.offset_px!=self.before_offset_px || !geometry.gestures.contains(&self.gesture)
            || [self.gesture.start_px,self.gesture.end_px].iter().any(|[x,y]|*x>=capture.size[0] || *y>=capture.size[1]) {
            return Err(InvalidRequest("Android scroll requires the observed source trajectory"));
        }
        self.request(&capture.generation,&capture.generation,1).map(|_|())
    }
}
#[cfg(test)]
#[path="android_scroll_tests.rs"]
mod tests;
