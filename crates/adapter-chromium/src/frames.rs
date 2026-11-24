//! Which state a screencast frame shows. The generation marks the wall time at which the page's state
//! changed — virtual time starting to move (the state shown afterwards is where it stops), a document
//! commit, an action — and frames are taken once the screencast has fallen quiet after a state, so a
//! frame shows the state of the last mark before it arrived.
use frontcap_core::time::VirtualTime;
use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq)]
pub struct Mark {
    /// Wall time, seconds since the Unix epoch, from which this state is shown.
    pub wall: f64,
    /// The virtual instant of the state; `None` while virtual time moves to it.
    pub t_us: Option<VirtualTime>,
    pub document: u64,
    /// State-changing events recorded so far.
    pub changes: u64,
    /// Whether a frame arrived while this was the latest state.
    pub framed: bool,
    /// Whether virtual time moved into this state (the renderer had frames to draw it with).
    pub moved: bool,
}

/// How late a frame can arrive after the state it shows: within this, a frame after several states
/// none of which has had a frame may show any of them.
pub const RENDER_LAG: f64 = 0.060;

#[derive(Debug, Default)]
pub struct Timeline {
    marks: VecDeque<Mark>,
}

/// Marks older than the last this many are dropped; frames older than them are not labelled.
const KEPT: usize = 4096;

impl Timeline {
    fn push(&mut self, mark: Mark) {
        self.marks.push_back(mark);
        while self.marks.len() > KEPT {
            self.marks.pop_front();
        }
    }

    /// Virtual time starts moving at `wall`; the state frames show from here is where it stops.
    pub fn moving(&mut self, wall: f64, document: u64, changes: u64) {
        self.push(Mark { wall, t_us: None, document, changes, framed: false, moved: true });
    }

    /// Virtual time stopped at `t`.
    pub fn stopped(&mut self, t: VirtualTime) {
        for m in self.marks.iter_mut().rev() {
            if m.t_us.is_some() {
                break;
            }
            m.t_us = Some(t);
        }
    }

    /// The state changed at `wall` while frozen (a document commit, an action).
    pub fn changed(&mut self, wall: f64, t: VirtualTime, document: u64, changes: u64) {
        self.push(Mark { wall, t_us: Some(t), document, changes, framed: false, moved: false });
    }

    /// The state a frame that arrived at `wall` shows.
    pub fn label(&self, wall: f64) -> Option<&Mark> {
        self.marks.iter().rev().find(|m| m.wall <= wall).or_else(|| self.marks.front())
    }

    /// The state a frame that arrived at `wall` shows, and whether that is certain: it is not when an
    /// earlier, different state began within `RENDER_LAG` before the arrival and never had a frame. The
    /// mark it is attributed to counts as framed.
    pub fn attribute(&mut self, wall: f64) -> Option<(Mark, bool)> {
        let i = self.marks.iter().rposition(|m| m.wall <= wall).unwrap_or(0);
        let mark = self.marks.get(i)?.clone();
        let exact = !self.marks.iter().take(i).rev().take_while(|m| m.wall >= wall - RENDER_LAG).any(|m| !m.framed && (m.t_us, m.document, m.changes) != (mark.t_us, mark.document, mark.changes));
        if let Some(m) = self.marks.get_mut(i) {
            m.framed = true;
        }
        Some((self.marks[i].clone(), exact))
    }

    /// The latest state, when the frame that arrived at `frame` shows it although none came after it:
    /// virtual time moved into that state, so the renderer had frames to draw every change since that
    /// frame (S10), and the screencast stayed quiet — nothing it shows changed. `None` when the latest
    /// state came from a change while frozen, whose frame may still be to come (S14).
    pub fn unchanged_since(&mut self, frame: f64) -> Option<Mark> {
        let last = self.marks.back_mut()?;
        if !last.moved || last.framed || last.t_us.is_none() || last.wall < frame {
            return None;
        }
        last.framed = true;
        Some(last.clone())
    }

    pub fn is_latest(&self, mark: &Mark) -> bool {
        self.marks.back().is_some_and(|b| (b.wall, b.t_us, b.document, b.changes) == (mark.wall, mark.t_us, mark.document, mark.changes))
    }

    /// Whether the latest state has had a frame.
    pub fn latest_framed(&self) -> bool {
        self.marks.back().is_some_and(|m| m.framed)
    }


    /// The wall time of the newest mark: frames that arrived before it are labelled for good.
    pub fn newest_wall(&self) -> f64 {
        self.marks.back().map_or(0.0, |m| m.wall)
    }
}

pub fn wall_now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

#[cfg(test)]
#[path = "frames_tests.rs"]
mod tests;
