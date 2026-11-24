use super::*;

/// Native identity of an activated surface and the main-thread paint it carries.
/// A compositor-only update can change the frame token without changing paint time.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct PresentedFrame {
    surface: String,
    frame: String,
    document: String,
    pub(super) painted_at: VirtualTime,
}

/// Evidence available while this controller observes rendering. It is deliberately
/// not reconstructed from a recorder journal after losing the native connection.
pub(super) struct Presentation {
    pub(super) frame: PresentedFrame,
    pub(super) changes: u64,
}

pub(super) struct SurfaceCopy {
    pub(super) png: Vec<u8>,
    pub(super) t_us: VirtualTime,
    /// The engine's paint instant of the copied surface.
    pub(super) painted_at: VirtualTime,
    pub(super) exact: bool,
    pub(super) current: bool,
}

/// Include files written before a recorder fault could append their acquisition
/// entry. A persisted controller counter alone would allow overwriting that evidence.
pub(super) fn last_surface_copy(bundle: &Bundle) -> std::io::Result<u64> {
    let mut last = 0;
    for dir in [bundle.screenshots_dir(), bundle.frames_dir()] {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("png") { continue; }
            if let Some(number) = path.file_stem().and_then(|s| s.to_str())
                .and_then(|s| s.rsplit_once('-')).and_then(|(_, number)| number.parse::<u64>().ok()) {
                last = last.max(number);
            }
        }
    }
    Ok(last)
}

pub(super) fn write_surface(path: &std::path::Path, png: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::OpenOptions::new().write(true).create_new(true).open(path)?.write_all(png)
}

impl Generation {
    pub(super) async fn presented_frame(&self) -> Result<PresentedFrame, ChromiumError> {
        self.presented_frame_after(None).await
    }

    /// The presented frame once the browser has activated the frame `after` (a render's
    /// `presentedFrameToken`) or a later one: the renderer answers a render on viz's acknowledgement, and the
    /// browser learns of the activation on another path, so an earlier read could name the frame before.
    pub(super) async fn presented_frame_after(&self, after: Option<String>) -> Result<PresentedFrame, ChromiumError> {
        let receipt = self.page.call(&page::GetPresentedFrameParams { after_frame_token: after }).await?;
        if !receipt.available { return Err(ChromiumError::NoFrame); }
        if !receipt.current_document { return Err(ChromiumError::StaleFrame); }
        let elapsed_us = (receipt.paint_time_ms - self.profile.initial_date_ms as f64) * 1000.0;
        if !elapsed_us.is_finite() || elapsed_us < 0.0 || elapsed_us.round() > self.now.0 as f64 {
            return Err(ChromiumError::Shape(format!("a native presentation outside the controlled timeline: paint={} ms, origin={} ms, elapsed={} us, boundary={} us", receipt.paint_time_ms, self.profile.initial_date_ms, elapsed_us, self.now.0)));
        }
        Ok(PresentedFrame {
            surface: receipt.surface_id, frame: receipt.frame_token, document: receipt.document_token,
            painted_at: VirtualTime(elapsed_us.round() as u64),
        })
    }

    /// How the presented surface relates to the current boundary, read from the engine without copying
    /// it: `current` when no main frame is wanted, `pending` when one is, `unavailable` without a surface
    /// of the current document. The baseline engine attributes presentations only by copying them.
    pub(super) async fn presented_relation(&mut self) -> Result<frontcap_core::observation::PresentationRelation, ChromiumError> {
        use frontcap_core::observation::PresentationRelation;
        if !self.patched {
            return Ok(PresentationRelation::Unavailable);
        }
        match self.presented_frame().await {
            Ok(_) if self.page.call(&emulation::IsMainFrameWantedParams::default()).await?.wanted => Ok(PresentationRelation::Pending),
            Ok(_) => Ok(PresentationRelation::Current),
            Err(ChromiumError::NoFrame | ChromiumError::StaleFrame) => Ok(PresentationRelation::Unavailable),
            Err(e) => Err(e),
        }
    }

    /// Copy an existing surface without requesting rendering. Native activation
    /// identity brackets the copy and the pending-render query, also after reconnect.
    pub(super) async fn copy_presented(&mut self) -> Result<SurfaceCopy, ChromiumError> {
        let before = self.presented_frame().await?;
        let png = self.copy_surface().await?;
        let current = !self.page.call(&emulation::IsMainFrameWantedParams::default()).await?.wanted;
        let after = self.presented_frame().await?;
        if before != after { return Err(ChromiumError::ObservationChanged("a frame was presented during the copy".into())); }
        let exact = current || self.presented.as_ref().is_some_and(|p| p.frame == before && p.changes == self.changes);
        Ok(SurfaceCopy { png, t_us: if current { self.now } else { before.painted_at }, painted_at: before.painted_at, exact, current })
    }
}
