//! What a process image of a generation's browser can carry, and letting the browser go for one.
use super::*;

impl Generation {
    /// What a process image of the browser cannot carry now, each named; empty when one can be taken. The
    /// browser's own state crosses whole; state with a peer outside its cell, or held by this controller
    /// across the boundary, does not.
    pub fn image_blockers(&self) -> Vec<String> {
        let mut blockers = Vec::new();
        if self.cell.is_none() {
            blockers.push("the browser runs outside a cell (a Linux host with FRONTCAP_CELLS)".to_string());
        }
        // Held mouse buttons, touch contacts, keys and a composition cross: the engine holds them, and a
        // controller that takes the browser again reads them from the trace.
        if self.held_pointer.as_ref().is_some_and(|hold| hold.html_drag) {
            blockers.push("an HTML drag in progress: its drag data belongs to this controller's session".into());
        }
        for passed in self.net.passthrough.values() {
            blockers.push(format!("pass-through request {} in flight: its connection leaves the cell", passed.ordinal));
        }
        if !self.net.stream_connections.is_empty() {
            blockers.push(format!("{} passed-through stream(s) open: their connections leave the cell", self.net.stream_connections.len()));
        }
        blockers
    }

    /// Video and audio elements playing in the generation's documents (its frames' included) and open
    /// popups', by context: they play by the real clock, which an image cannot carry to a child as the
    /// page's time. Media the DOM does not name (Web Audio, OffscreenCanvas) is not found here.
    pub async fn playing_media(&mut self) -> Result<Vec<String>, ChromiumError> {
        // The observer world reads every frame's document, another origin's included.
        const PLAYING: &str = r#"(() => {
            const found = [];
            const visit = (doc) => {
                for (const m of doc.querySelectorAll('video, audio')) if (!m.paused && !m.ended) found.push(m.tagName.toLowerCase());
                for (const f of doc.querySelectorAll('iframe, frame')) { try { if (f.contentDocument) visit(f.contentDocument); } catch {} }
            };
            visit(document);
            return found;
        })()"#;
        let mut playing = Vec::new();
        let tags = |v: serde_json::Value| -> Vec<String> { v.as_array().into_iter().flatten().filter_map(|t| t.as_str().map(str::to_string)).collect() };
        for tag in tags(self.world_eval(PLAYING).await?) {
            playing.push(format!("{tag} playing in page"));
        }
        let popups: Vec<String> = self.contexts().await?.into_iter().filter(|c| c.open && c.opener.is_some()).map(|c| c.id).collect();
        for popup in popups {
            self.enter_context(&popup)?;
            let found = self.world_eval(PLAYING).await;
            self.leave_context();
            for tag in tags(found?) {
                playing.push(format!("{tag} playing in {popup}"));
            }
        }
        Ok(playing)
    }

    /// Lets the browser go for a process image, as a controller that stops does, and returns where it
    /// is: the engine keeps its tab-owned state, and `adopt` takes the browser (or a restored copy) again.
    pub fn let_go(self) -> Result<Endpoint, ChromiumError> {
        if self.cell.is_none() {
            return Err(ChromiumError::Unsupported("a process image needs the browser in a cell".into()));
        }
        let endpoint = self.endpoint();
        self.abandon();
        Ok(endpoint)
    }
}
