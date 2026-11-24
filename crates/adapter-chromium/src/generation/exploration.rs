//! Coherent reads of a stopped generation; no rendering or clock grants are hidden in observation.
use super::*;
use frontcap_core::observation::{ImageAnchor, ObservationBoundary, PresentationRelation, Scene, ScenePresentation};

pub(super) fn affects_state(kind: &EventKind) -> bool {
    // A rule installation changes how future requests are answered, not the page: an observation's
    // boundary stays current across it.
    !matches!(kind, EventKind::Watch(_) | EventKind::Console(_) | EventKind::Controller(_) | EventKind::RulesInstalled(_))
        && !matches!(kind, EventKind::Clock(c) if c.phase == ClockPhase::Start)
}

impl Generation {
    /// Frontcap and native input use viewport CSS coordinates; DOM.getNodeForLocation
    /// takes document CSS coordinates. Read the current offset without granting time.
    pub(super) async fn hit_test_viewport(&self, point: [f64; 2]) -> Result<dom::GetNodeForLocationReturns, ChromiumError> {
        let viewport = self.page.call(&page::GetLayoutMetricsParams::default()).await?.css_visual_viewport;
        let x = (point[0] + viewport.page_x).round() as i64;
        let y = (point[1] + viewport.page_y).round() as i64;
        self.page.call(&dom::GetNodeForLocationParams::new(x, y)).await
    }

    pub(super) async fn checked_pointer_point(&mut self, target: &Locator, expected: &ObservationBoundary, point: [f64; 2]) -> Result<(), ChromiumError> {
        let located = self.locate(expected, point).await?;
        let node = self.resolve(target).await?;
        let object = self.input_object(node).await?;
        let hit = self.resolve(&Locator::Ref { id: located.target.r#ref }).await?;
        let hit_object = self.world_object(hit).await?;
        if self.call_on(&object, ACCEPTED, vec![hit_object]).await? != serde_json::Value::Bool(true) {
            return Err(ChromiumError::NotActionable("the point hits another target; locate it again".into()));
        }
        if self.call_on(&object, "function() { return globalThis.__frontcap.enabled(this) && globalThis.__frontcap.visible(this); }", vec![]).await? != serde_json::Value::Bool(true) {
            return Err(ChromiumError::NotActionable("target is disabled or not visible".into()));
        }
        self.expect_boundary(expected).await?;
        Ok(())
    }

    fn retain_image(&mut self, boundary: &ObservationBoundary, capture: &frontcap_core::observation::Captured) -> Result<ImageAnchor, ChromiumError> {
        let bytes = std::fs::read(&capture.path)?;
        let reader = png::Decoder::new(std::io::Cursor::new(&bytes)).read_info()
            .map_err(|e| ChromiumError::Shape(format!("capture PNG: {e}")))?;
        let size = [reader.info().width, reader.info().height];
        self.obs.next_image += 1;
        let image = ImageAnchor {
            id: format!("{}-image-{}", self.launch_id, self.obs.next_image),
            boundary: boundary.clone(), capture: capture.clone(), pixel_size: size, source_size: size,
            viewport_size: [self.profile.viewport.width, self.profile.viewport.height],
            clip: [0, 0, size[0], size[1]], sha256: frontcap_core::canonical::sha256_bytes_hex(&bytes),
        };
        self.remember_image(image.clone());
        Ok(image)
    }

    fn remember_image(&mut self, image: ImageAnchor) {
        self.obs.images.push_back(image);
        while self.obs.images.len() > 16 { self.obs.images.pop_front(); }
    }

    fn retained_image(&self, id: &str) -> Result<(ImageAnchor, Vec<u8>), ChromiumError> {
        let image = self.obs.images.iter().find(|image| image.id == id)
            .ok_or_else(|| ChromiumError::InvalidArgument("unknown or expired image; acquire a fresh scene".into()))?.clone();
        let bytes = std::fs::read(&image.capture.path)?;
        if frontcap_core::canonical::sha256_bytes_hex(&bytes) != image.sha256 {
            return Err(ChromiumError::InvalidArgument("retained image content changed; acquire a fresh scene".into()));
        }
        Ok((image, bytes))
    }

    pub async fn locate_image(&mut self, id: &str, point: [f64; 2]) -> Result<frontcap_core::observation::Located, ChromiumError> {
        let (image, _) = self.retained_image(id)?;
        if !image.capture.current || !image.capture.exact {
            return Err(ChromiumError::ObservationChanged(format!("image {id} is not an exact copy of the current presentation")));
        }
        self.expect_boundary(&image.boundary).await?;
        if point.iter().zip(image.pixel_size).any(|(p, size)| !p.is_finite() || *p < 0.0 || *p >= f64::from(size)) {
            return Err(ChromiumError::InvalidArgument("image point must be inside the retained image in pixels".into()));
        }
        let css = [
            (f64::from(image.clip[0]) + point[0]) * f64::from(image.viewport_size[0]) / f64::from(image.source_size[0]),
            (f64::from(image.clip[1]) + point[1]) * f64::from(image.viewport_size[1]) / f64::from(image.source_size[1]),
        ];
        self.locate(&image.boundary, css).await
    }

    /// Element geometry belongs to the current logical boundary, so only a current presentation
    /// can supply its pixels. Never silently clip an offscreen element or scroll to acquire it.
    pub async fn screenshot_target(&mut self, target: &Locator) -> Result<ImageAnchor, ChromiumError> {
        let inspected = self.inspect(target, &[Prop::Bbox]).await?;
        let bbox = &inspected.values["bbox"];
        let coord = |key: &str| bbox[key].as_f64().filter(|v| v.is_finite())
            .ok_or_else(|| ChromiumError::InvalidArgument("target has no finite capture geometry".into()));
        let [x, y, width, height] = [coord("x")?, coord("y")?, coord("width")?, coord("height")?];
        let vw = f64::from(self.profile.viewport.width);
        let vh = f64::from(self.profile.viewport.height);
        if width <= 0.0 || height <= 0.0 || x < 0.0 || y < 0.0 || x + width > vw || y + height > vh {
            return Err(ChromiumError::InvalidArgument("capture target must fit inside the presented viewport; scroll and progress explicitly first".into()));
        }
        let capture = self.screenshot().await?;
        self.expect_boundary(&inspected.boundary).await?;
        if !capture.current || !capture.exact {
            return Err(ChromiumError::ObservationChanged("target geometry has no exact current presentation; progress explicitly first".into()));
        }
        let source = self.retain_image(&inspected.boundary, &capture)?;
        let sx = f64::from(source.pixel_size[0]) / vw;
        let sy = f64::from(source.pixel_size[1]) / vh;
        let left = (x * sx).floor() as u32;
        let top = (y * sy).floor() as u32;
        let right = ((x + width) * sx).ceil().min(f64::from(source.pixel_size[0])) as u32;
        let bottom = ((y + height) * sy).ceil().min(f64::from(source.pixel_size[1])) as u32;
        self.crop_image(&source.id, [left, top, right - left, bottom - top])
    }

    /// Crop retained pixels only. This remains useful for historical images and never renders.
    pub fn crop_image(&mut self, id: &str, rect: [u32; 4]) -> Result<ImageAnchor, ChromiumError> {
        let (source, bytes) = self.retained_image(id)?;
        let [x, y, width, height] = rect;
        if width == 0 || height == 0 || x.checked_add(width).is_none_or(|end| end > source.pixel_size[0])
            || y.checked_add(height).is_none_or(|end| end > source.pixel_size[1]) {
            return Err(ChromiumError::InvalidArgument("crop must be a nonempty rectangle inside the retained image".into()));
        }
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().map_err(|e| ChromiumError::Shape(e.to_string()))?;
        let size = reader.output_buffer_size().ok_or_else(|| ChromiumError::InvalidArgument("image too large to crop".into()))?;
        let mut pixels = vec![0; size];
        let info = reader.next_frame(&mut pixels).map_err(|e| ChromiumError::Shape(e.to_string()))?;
        let channels = info.color_type.samples();
        let stride = width as usize * channels;
        let mut cropped = Vec::with_capacity(stride * height as usize);
        for row in y..y + height {
            let start = row as usize * info.line_size + x as usize * channels;
            cropped.extend_from_slice(&pixels[start..start + stride]);
        }
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, width, height);
            encoder.set_color(info.color_type);
            encoder.set_depth(info.bit_depth);
            let mut writer = encoder.write_header().map_err(|e| ChromiumError::Shape(e.to_string()))?;
            writer.write_image_data(&cropped).map_err(|e| ChromiumError::Shape(e.to_string()))?;
        }
        self.obs.next_image += 1;
        let id = format!("{}-image-{}", self.launch_id, self.obs.next_image);
        let path = self.bundle.screenshots_dir().join(format!("{id}.png"));
        std::fs::write(&path, &encoded)?;
        let mut image = source;
        image.id = id;
        image.capture.path = path;
        image.pixel_size = [width, height];
        image.clip = [image.clip[0] + x, image.clip[1] + y, width, height];
        image.sha256 = frontcap_core::canonical::sha256_bytes_hex(&encoded);
        let record = frontcap_core::event::ScreenshotRecord {
            path: format!("screenshots/{}.png", image.id), sha256: image.sha256.clone(),
            source_frame: image.capture.frame, frame_t_us: image.capture.t_us, document: image.capture.document,
        };
        self.acquisition.append(self.log.next_seq().saturating_sub(1), self.now, AcquisitionKind::Screenshot(record))?;
        self.remember_image(image.clone());
        Ok(image)
    }

    /// Native hit-testing in top-viewport CSS coordinates, never scrolling or dispatching input.
    pub async fn locate(&mut self, expected: &ObservationBoundary, point: [f64; 2]) -> Result<frontcap_core::observation::Located, ChromiumError> {
        let [x, y] = point;
        if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0
            || x >= f64::from(self.profile.viewport.width) || y >= f64::from(self.profile.viewport.height) {
            return Err(ChromiumError::InvalidArgument("locate point must be inside the viewport in CSS pixels".into()));
        }
        self.expect_boundary(expected).await?;
        // CDP's native hit test accepts integral CSS coordinates. Report the point actually tested.
        let point = [x.floor(), y.floor()];
        let hit = self.hit_test_viewport(point).await?;
        let object = self.observed_object(hit.backend_node_id.clone()).await?;
        let tag = self.call_on(&object, "function() { return this.localName || ''; }", vec![]).await?
            .as_str().unwrap_or_default().to_string();
        let reference = self.ref_for(hit.backend_node_id.0);
        let target = self.inspect(&Locator::Ref { id: reference }, &[Prop::Bbox, Prop::Interaction]).await?;
        if &target.boundary != expected {
            return Err(ChromiumError::ObservationChanged(target.boundary.changes_from(expected)));
        }
        let service_clock = self.page.service_clock();
        let hit_path = service_clock.timeout(Session::CALL_TIMEOUT, self.hit_path(&hit)).await
            .map_err(|_| ChromiumError::CallTimeout("hit ancestry")).and_then(|path| path);
        // A disappearing node can make a native ancestry query fail before the complete path exists.
        // Prefer the changed observation boundary when it explains that failure.
        self.expect_boundary(expected).await?;
        let hit_path = Some(hit_path?);
        Ok(frontcap_core::observation::Located { point: [point[0] as i32, point[1] as i32], target, canvas: tag == "canvas", tag, hit_path })
    }

    /// Composed ancestry, with native slot assignment and explicit frame-document joins. Never grants
    /// time or retries a disappearing node. The caller checks its observation boundary after the read.
    async fn hit_path(&mut self, hit: &dom::GetNodeForLocationReturns) -> Result<frontcap_core::observation::HitPath, ChromiumError> {
        use frontcap_core::observation::{HitPath, HitPathNode, HitPathNodeKind as Kind, HitPathOmission as Omission, HitPathRelation as Via};
        const LIMIT: u32 = 512;
        let tree = self.page.call(&page::GetFrameTreeParams::default()).await?.frame_tree;
        let top = tree.frame.id.clone();
        let mut parents = BTreeMap::new();
        let mut stack = vec![tree];
        while let Some(tree) = stack.pop() {
            for child in tree.child_frames.into_iter().flatten() {
                parents.insert(child.frame.id.0.clone(), tree.frame.id.clone());
                stack.push(child);
            }
        }
        let mut frame = hit.frame_id.clone();
        let mut node = self.page.call(&dom::DescribeNodeParams {
            backend_node_id: Some(hit.backend_node_id.clone()), depth: Some(1), ..Default::default()
        }).await?.node;
        let mut path = HitPath { nodes: Vec::new(), omission: None };
        let mut via = Via::Hit;
        loop {
            if path.nodes.len() == LIMIT as usize {
                path.omission = Some(Omission::DepthLimit { limit: LIMIT });
                return Ok(path);
            }
            let kind = match node.node_type {
                1 => Kind::Element, 3 => Kind::Text, 9 => Kind::Document,
                11 if node.shadow_root_type.is_some() => Kind::ShadowRoot,
                _ => Kind::Other,
            };
            path.nodes.push(HitPathNode {
                kind,
                name: if node.local_name.is_empty() { node.node_name.clone() } else { node.local_name.clone() },
                r#ref: matches!(kind, Kind::Element | Kind::Text).then(|| self.ref_for(node.backend_node_id.0)),
                via,
            });
            if kind == Kind::Document {
                if frame == top { return Ok(path) }
                let Some(parent) = parents.get(&frame.0) else {
                    path.omission = Some(Omission::InaccessibleBoundary { detail: "the hit frame has no parent in the current frame tree".into() });
                    return Ok(path);
                };
                let owner = match self.page.call(&dom::GetFrameOwnerParams::new(frame.clone())).await {
                    Ok(owner) => owner.backend_node_id,
                    Err(ChromiumError::Cdp(cdp::CdpError::Protocol { message, .. })) => {
                        path.omission = Some(Omission::InaccessibleBoundary { detail: message });
                        return Ok(path);
                    }
                    Err(error) => return Err(error),
                };
                node = self.page.call(&dom::DescribeNodeParams { backend_node_id: Some(owner), depth: Some(1), ..Default::default() }).await?.node;
                frame = parent.clone();
                via = Via::FrameOwner;
                continue;
            }
            // Native assignment is essential: a slotted node's JS assignedSlot is null when its
            // assigned slot belongs to a closed root, even though the composed path includes it.
            if let Some(slot) = node.assigned_slot {
                node = self.page.call(&dom::DescribeNodeParams {
                    backend_node_id: Some(slot.backend_node_id), depth: Some(1), ..Default::default()
                }).await?.node;
                via = Via::AssignedSlot;
                continue;
            }
            let object = match self.world_object(node.backend_node_id).await {
                Ok(object) => object,
                Err(ChromiumError::Unsupported(detail)) => {
                    path.omission = Some(Omission::InaccessibleBoundary { detail });
                    return Ok(path);
                }
                Err(error) => return Err(error),
            };
            let parent = self.call_for_object(&object, "function() { return globalThis.__frontcap.hitParent(this); }").await;
            let _ = self.page.call(&runtime::ReleaseObjectParams::new(object)).await;
            let Some(parent) = parent? else {
                path.omission = Some(Omission::Detached);
                return Ok(path);
            };
            let described = self.page.call(&dom::DescribeNodeParams { object_id: Some(parent.clone()), depth: Some(1), ..Default::default() }).await;
            let _ = self.page.call(&runtime::ReleaseObjectParams::new(parent)).await;
            node = described?.node;
            via = if kind == Kind::ShadowRoot { Via::ShadowHost } else { Via::Parent };
        }
    }

    /// Detailed properties and the actual click checks, all read at one boundary.
    pub async fn inspect(&mut self, target: &Locator, props: &[Prop]) -> Result<frontcap_core::observation::Inspection, ChromiumError> {
        use frontcap_core::observation::{ClickAssessment, Inspection};
        if props.len() > 64 {
            return Err(ChromiumError::InvalidArgument("inspect accepts at most 64 properties".into()));
        }
        let boundary = self.observation_boundary().await?;
        let queried = self.query(target, props).await?;
        // Resolve the reference issued by this query, not a second potentially ambiguous locator.
        let reference = Locator::Ref { id: queried.r#ref.clone() };
        let node = self.resolve(&reference).await?;
        let border_quad = match self.page.call(&dom::GetBoxModelParams { backend_node_id: Some(node), ..Default::default() }).await {
            Ok(m) => Some(m.model.border.0),
            Err(ChromiumError::Cdp(cdp::CdpError::Protocol { message, .. })) if message.contains("Could not compute box model") => None,
            Err(e) => return Err(e),
        };
        let click = match self.pointer_target(&reference, true).await {
            Ok((_, (x, y))) => ClickAssessment { ready: true, point: Some([x, y]), reason: None },
            Err(ChromiumError::NotActionable(reason)) => ClickAssessment { ready: false, point: None, reason: Some(reason) },
            Err(ChromiumError::Offscreen { .. }) => ClickAssessment { ready: false, point: None, reason: Some("offscreen".into()) },
            Err(e) => return Err(e),
        };
        self.expect_boundary(&boundary).await?;
        Ok(Inspection { boundary, r#ref: queried.r#ref, values: queried.values, border_quad, click })
    }

    fn observed_boundary(&self) -> ObservationBoundary {
        ObservationBoundary {
            launch_id: self.launch_id.clone(),
            state_revision: self.state_revision,
            t_us: self.now,
            context_id: format!("context-{}", self.target_id.to_ascii_lowercase()),
            document_id: format!("document-{}", self.documents),
        }
    }

    /// A barrier and a fresh boundary, without advancing the page or demanding a frame.
    pub async fn observation_boundary(&mut self) -> Result<ObservationBoundary, ChromiumError> {
        self.alive()?;
        self.drain().await?;
        self.commit()?;
        Ok(self.observed_boundary())
    }

    /// Refuses unless a fresh boundary is `expected`, naming what changed.
    pub(super) async fn expect_boundary(&mut self, expected: &ObservationBoundary) -> Result<(), ChromiumError> {
        let now = self.observation_boundary().await?;
        if &now != expected { return Err(ChromiumError::ObservationChanged(now.changes_from(expected))); }
        Ok(())
    }

    pub async fn scene(&mut self, scope: Option<&Locator>, depth: Option<u32>, max_nodes: u32, image: bool, unfold: bool) -> Result<Scene, ChromiumError> {
        if max_nodes == 0 || max_nodes > 2000 {
            return Err(ChromiumError::InvalidArgument("scene max_nodes must be between 1 and 2000".into()));
        }
        let boundary = self.observation_boundary().await?;
        let snapshot = self.snapshot_with_geometry(scope, depth, max_nodes, false, unfold).await?;
        let capture = if image {
            match self.screenshot().await {
                Ok(s) => Some(s),
                Err(ChromiumError::NoFrame | ChromiumError::StaleFrame) => None,
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        // Without an image the relation is still the presented surface's, not the absence of a copy.
        let unimaged = if image { None } else { Some(self.presented_relation().await?) };
        self.expect_boundary(&boundary).await?;
        let relation = match (&capture, unimaged) {
            (_, Some(relation)) => relation,
            (Some(s), None) if s.current => PresentationRelation::Current,
            (Some(_), None) => PresentationRelation::Pending,
            (None, None) => PresentationRelation::Unavailable,
        };
        let mut coverage_gaps = vec![
            "video, OffscreenCanvas, audio, shared workers and worklets are not verified frozen".into(),
            if self.profile.popups.is_block() {
                "windows the page opens are refused (popups: block); context coverage is not exhaustive".into()
            } else {
                "a window opened by a modifier click opens at the first boundary after it is announced; context coverage is not exhaustive".into()
            },
        ];
        if !self.patched {
            coverage_gaps.push("baseline engine: first attachment of open shadow roots is not independently intercepted".into());
            coverage_gaps.push("baseline engine: closed shadow roots are not observed".into());
            coverage_gaps.push("baseline engine: presentation and cross-origin observation have weaker guarantees".into());
        }
        let image = capture.as_ref().map(|capture| self.retain_image(&boundary, capture)).transpose()?;
        Ok(Scene {
            boundary, snapshot, presentation: ScenePresentation { relation, capture, image },
            engine_revision: self.engine_revision.clone(), coverage_gaps,
        })
    }
}
